use crate::ir::ContentPart;
use crate::middleware::{BoxFuture, Middleware, MiddlewareContext, MiddlewareRejection, NextFn};
use dashmap::DashMap;

/// Context & Episodic Memory Injection Middleware.
/// - Pre-leg: appends configured workspace facts + recent episodic memory entries into
///   `ctx.request.system` without mutating existing `cache_control` breakpoints.
/// - Post-leg: records bounded response takeaways into the local workspace memory bank.
pub struct ContextMemoryMiddleware {
    enabled: bool,
    max_entries_per_workspace: usize,
    static_facts: Vec<String>,
    episodic: DashMap<String, Vec<String>>,
}

impl ContextMemoryMiddleware {
    pub fn new(enabled: bool, static_facts: Vec<String>, max_entries_per_workspace: usize) -> Self {
        Self {
            enabled,
            max_entries_per_workspace: max_entries_per_workspace.max(1),
            static_facts,
            episodic: DashMap::new(),
        }
    }

    pub fn record_fact(&self, workspace: &str, fact: String) {
        let mut entry = self.episodic.entry(workspace.to_string()).or_default();
        if !entry.contains(&fact) {
            entry.push(fact);
            if entry.len() > self.max_entries_per_workspace {
                let overflow = entry.len() - self.max_entries_per_workspace;
                entry.drain(0..overflow);
            }
        }
    }

    pub fn clear(&self) {
        self.episodic.clear();
    }
}

impl Middleware for ContextMemoryMiddleware {
    fn name(&self) -> &'static str {
        "context_memory"
    }

    fn handle<'a>(
        &'a self,
        mut ctx: MiddlewareContext,
        next: NextFn<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>> {
        Box::pin(async move {
            if !self.enabled {
                return next(ctx).await;
            }

            let mut lines: Vec<String> = self.static_facts.clone();
            if let Some(mem) = self.episodic.get(&ctx.workspace) {
                lines.extend(mem.iter().cloned());
            }

            if !lines.is_empty() {
                let count = lines.len();
                let block = format!(
                    "[LoomGate Workspace Memory]\n{}",
                    lines
                        .into_iter()
                        .map(|l| format!("- {}", l))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                ctx.request.system.push(ContentPart::Text {
                    text: block,
                    cache_control: None,
                });
                ctx.annotations
                    .insert("X-Gateway-Memory-Injected".into(), count.to_string());
            }

            let ctx = next(ctx).await?;

            // Post-leg: capture a concise summary from non-streaming response
            if let Some(ref resp) = ctx.response {
                let text = resp.concatenated_text();
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let snippet: String = trimmed.chars().take(160).collect();
                    self.record_fact(&ctx.workspace, snippet);
                }
            }

            Ok(ctx)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamProtocol;
    use crate::ir::{CanonicalMessage, CanonicalRequest, CanonicalResponse, CanonicalUsage, Role};
    use crate::middleware::MiddlewarePipeline;
    use std::sync::Arc;

    #[tokio::test]
    async fn injects_static_and_episodic_memory_across_turns() {
        let mem = Arc::new(ContextMemoryMiddleware::new(
            true,
            vec!["Workspace uses Rust 2021 edition.".into()],
            5,
        ));
        let mut pipe = MiddlewarePipeline::new();
        pipe.push(mem);

        let make_ctx = || {
            let req = CanonicalRequest {
                model: "gpt-4o".into(),
                system: vec![],
                messages: vec![CanonicalMessage {
                    role: Role::User,
                    parts: vec![ContentPart::Text {
                        text: "hi".into(),
                        cache_control: None,
                    }],
                }],
                tools: vec![],
                tool_choice: None,
                max_tokens: None,
                temperature: None,
                top_p: None,
                stream: false,
                reasoning: None,
                opaque_state: Default::default(),
                session_id: None,
                task_stage: None,
                extra_fields: Default::default(),
            };
            MiddlewareContext::new(
                "r".into(),
                UpstreamProtocol::OpenAi,
                "c".into(),
                "repo-a".into(),
                req,
            )
        };

        let ctx1 = make_ctx();
        let (_c1, r1) = pipe
            .execute(ctx1, |mut c| {
                Box::pin(async move {
                    assert_eq!(c.request.system.len(), 1);
                    c.response = Some(CanonicalResponse {
                        id: "1".into(),
                        model: "gpt-4o".into(),
                        parts: vec![ContentPart::Text {
                            text: "Decided on SQLite WAL.".into(),
                            cache_control: None,
                        }],
                        stop_reason: Some("end_turn".into()),
                        usage: CanonicalUsage::default(),
                    });
                    (c, Ok::<_, ()>(()))
                })
            })
            .await
            .unwrap();
        r1.unwrap();

        let ctx2 = make_ctx();
        let (final_ctx2, r2) = pipe
            .execute(ctx2, |c| {
                Box::pin(async move {
                    match &c.request.system[0] {
                        ContentPart::Text { text, .. } => {
                            assert!(text.contains("Workspace uses Rust 2021 edition."));
                            assert!(text.contains("Decided on SQLite WAL."));
                        }
                        _ => panic!("expected memory text"),
                    }
                    (c, Ok::<_, ()>(()))
                })
            })
            .await
            .unwrap();
        r2.unwrap();

        assert_eq!(
            final_ctx2
                .annotations
                .get("X-Gateway-Memory-Injected")
                .map(String::as_str),
            Some("2")
        );
    }
}
