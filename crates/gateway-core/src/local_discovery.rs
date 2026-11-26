use crate::config::{normalize_base_url, ChannelConfig, UpstreamProtocol};
use futures_util::future::join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Short per-probe timeout (<= 400ms) so scanning local inference runtimes never blocks the UI.
const PROBE_TIMEOUT_MS: u64 = 350;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalProviderKind {
    Ollama,
    LmStudio,
    Vllm,
    CustomOpenAi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalProbeSpec {
    pub kind: LocalProviderKind,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    pub probe_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredLocalProvider {
    pub kind: LocalProviderKind,
    pub display_name: String,
    pub suggested_channel_id: String,
    pub base_url: String,
    pub port: u16,
    pub models: Vec<String>,
    pub latency_ms: u64,
}

pub fn default_probe_specs() -> Vec<LocalProbeSpec> {
    vec![
        LocalProbeSpec {
            kind: LocalProviderKind::Ollama,
            display_name: "Ollama".into(),
            host: "127.0.0.1".into(),
            port: 11434,
            probe_path: "/api/tags".into(),
        },
        LocalProbeSpec {
            kind: LocalProviderKind::LmStudio,
            display_name: "LM Studio".into(),
            host: "127.0.0.1".into(),
            port: 1234,
            probe_path: "/v1/models".into(),
        },
        LocalProbeSpec {
            kind: LocalProviderKind::Vllm,
            display_name: "vLLM".into(),
            host: "127.0.0.1".into(),
            port: 8000,
            probe_path: "/v1/models".into(),
        },
    ]
}

fn parse_models_from_response(val: &Value) -> Vec<String> {
    // Ollama `/api/tags`: { "models": [{ "name": "qwen2.5-coder:7b", ... }] }
    if let Some(arr) = val.get("models").and_then(|v| v.as_array()) {
        let mut names: Vec<String> = arr
            .iter()
            .filter_map(|m| {
                m.get("name")
                    .or_else(|| m.get("model"))
                    .and_then(|s| s.as_str())
                    .map(String::from)
            })
            .collect();
        names.sort();
        names.dedup();
        if !names.is_empty() {
            return names;
        }
    }

    // OpenAI-compatible `/v1/models` (LM Studio, vLLM, llama.cpp): { "data": [{ "id": "..." }] }
    if let Some(arr) = val.get("data").and_then(|v| v.as_array()) {
        let mut ids: Vec<String> = arr
            .iter()
            .filter_map(|m| m.get("id").and_then(|s| s.as_str()).map(String::from))
            .collect();
        ids.sort();
        ids.dedup();
        return ids;
    }

    Vec::new()
}

fn suggested_id_for(kind: &LocalProviderKind, port: u16) -> String {
    match kind {
        LocalProviderKind::Ollama => {
            if port == 11434 {
                "local-ollama".into()
            } else {
                format!("local-ollama-{}", port)
            }
        }
        LocalProviderKind::LmStudio => {
            if port == 1234 {
                "local-lmstudio".into()
            } else {
                format!("local-lmstudio-{}", port)
            }
        }
        LocalProviderKind::Vllm => {
            if port == 8000 {
                "local-vllm".into()
            } else {
                format!("local-vllm-{}", port)
            }
        }
        LocalProviderKind::CustomOpenAi => format!("local-openai-{}", port),
    }
}

async fn probe_single(client: &Client, spec: LocalProbeSpec) -> Option<DiscoveredLocalProvider> {
    let base_url = format!("http://{}:{}", spec.host, spec.port);
    let url = format!("{}{}", base_url, spec.probe_path);
    let started = Instant::now();

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        // For custom ports, retry `/api/tags` if `/v1/models` returned 404
        if spec.kind == LocalProviderKind::CustomOpenAi && spec.probe_path == "/v1/models" {
            let alt_url = format!("{}/api/tags", base_url);
            let alt_resp = client.get(&alt_url).send().await.ok()?;
            if !alt_resp.status().is_success() {
                return None;
            }
            let body: Value = alt_resp.json().await.ok()?;
            let models = parse_models_from_response(&body);
            return Some(DiscoveredLocalProvider {
                kind: LocalProviderKind::Ollama,
                display_name: format!("Ollama (:{})", spec.port),
                suggested_channel_id: suggested_id_for(&LocalProviderKind::Ollama, spec.port),
                base_url: normalize_base_url(&base_url),
                port: spec.port,
                models,
                latency_ms: started.elapsed().as_millis() as u64,
            });
        }
        return None;
    }

    let body: Value = resp.json().await.ok()?;
    let models = parse_models_from_response(&body);
    let latency_ms = started.elapsed().as_millis() as u64;

    Some(DiscoveredLocalProvider {
        suggested_channel_id: suggested_id_for(&spec.kind, spec.port),
        kind: spec.kind,
        display_name: spec.display_name,
        base_url: normalize_base_url(&base_url),
        port: spec.port,
        models,
        latency_ms,
    })
}

/// Probes default local inference runtimes (Ollama 11434, LM Studio 1234, vLLM 8000) plus any
/// user-specified custom localhost ports in parallel with a hard <= 350ms timeout.
pub async fn discover_local_providers(custom_ports: &[u16]) -> Vec<DiscoveredLocalProvider> {
    let mut specs = default_probe_specs();
    for &port in custom_ports {
        if port == 0 || specs.iter().any(|s| s.port == port) {
            continue;
        }
        specs.push(LocalProbeSpec {
            kind: LocalProviderKind::CustomOpenAi,
            display_name: format!("Local OpenAI-Compatible (:{})", port),
            host: "127.0.0.1".into(),
            port,
            probe_path: "/v1/models".into(),
        });
    }
    discover_specs(specs).await
}

pub async fn discover_specs(specs: Vec<LocalProbeSpec>) -> Vec<DiscoveredLocalProvider> {
    let client = match Client::builder()
        .timeout(Duration::from_millis(PROBE_TIMEOUT_MS))
        .connect_timeout(Duration::from_millis(PROBE_TIMEOUT_MS))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let futs = specs.into_iter().map(|s| probe_single(&client, s));
    let mut found: Vec<DiscoveredLocalProvider> = join_all(futs)
        .await
        .into_iter()
        .flatten()
        .collect();
    found.sort_by_key(|p| p.port);
    found
}

/// Converts a discovered local provider into an OpenAI-compatible `ChannelConfig` ready to upsert
/// into the live P2C pool and persist to `config.local.toml`.
pub fn build_local_channel_config(
    provider: &DiscoveredLocalProvider,
    channel_id_override: Option<String>,
    offline_fallback: bool,
) -> ChannelConfig {
    let id = channel_id_override
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| provider.suggested_channel_id.clone());

    let mut cfg = ChannelConfig {
        id,
        protocol: UpstreamProtocol::OpenAi,
        base_url: provider.base_url.clone(),
        api_key: "local-inference".into(),
        weight: 100,
        enabled: true,
        models: provider.models.clone(),
        model_map: HashMap::new(),
        proxy_url: None,
        fallback_models: vec![],
        quota_endpoint: None,
        offline_fallback,
    };
    cfg.normalize();
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_json_server(body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..4 {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn parallel_discovery_detects_mock_ollama_and_lm_studio_under_400ms() {
        let ollama_port = spawn_mock_json_server(
            r#"{"models":[{"name":"qwen2.5-coder:7b"},{"name":"deepseek-r1:8b"}]}"#,
        )
        .await;
        let lmstudio_port = spawn_mock_json_server(
            r#"{"object":"list","data":[{"id":"llama-3.3-70b-instruct"}]}"#,
        )
        .await;

        let specs = vec![
            LocalProbeSpec {
                kind: LocalProviderKind::Ollama,
                display_name: "Ollama".into(),
                host: "127.0.0.1".into(),
                port: ollama_port,
                probe_path: "/api/tags".into(),
            },
            LocalProbeSpec {
                kind: LocalProviderKind::LmStudio,
                display_name: "LM Studio".into(),
                host: "127.0.0.1".into(),
                port: lmstudio_port,
                probe_path: "/v1/models".into(),
            },
        ];

        let started = Instant::now();
        let found = discover_specs(specs).await;
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "discovery exceeded 400ms budget"
        );
        assert_eq!(found.len(), 2);

        let ol = found.iter().find(|p| p.kind == LocalProviderKind::Ollama).unwrap();
        assert_eq!(ol.models, vec!["deepseek-r1:8b", "qwen2.5-coder:7b"]);
        let lm = found.iter().find(|p| p.kind == LocalProviderKind::LmStudio).unwrap();
        assert_eq!(lm.models, vec!["llama-3.3-70b-instruct"]);

        let ch = build_local_channel_config(ol, None, true);
        assert!(ch.offline_fallback);
        assert_eq!(ch.protocol, UpstreamProtocol::OpenAi);
        assert_eq!(ch.models.len(), 2);
    }
}
