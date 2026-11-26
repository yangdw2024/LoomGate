use crate::config::ModelPricing;
use crate::middleware::guardrail::redact_sensitive_text;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum TrajectoryError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("step not found: {0}")]
    NotFound(String),
}

/// Immutable append-only trajectory step stored locally in SQLite (WAL mode).
/// Sensitive prompts remain strictly local on the developer's machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryStep {
    pub step_id: String,
    pub workspace: String,
    pub session_id: String,
    pub parent_step_id: Option<String>,
    pub turn_index: i64,
    pub protocol: String,
    pub channel_id: String,
    pub requested_model: String,
    pub actual_model: String,
    pub route_mode: String,
    pub sticky_hit: bool,
    pub ttft_ms: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub status: String,
    pub request_json: String,
    pub response_text: String,
    pub tool_calls_json: String,
    pub created_at_ms: i64,
}

impl TrajectoryStep {
    pub fn cache_hit_ratio(&self) -> f64 {
        let total = self.input_tokens + self.cache_read_tokens;
        if total == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / total as f64
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub workspace: String,
    pub step_count: i64,
    pub fork_count: i64,
    pub total_cache_read_tokens: i64,
    pub total_input_tokens: i64,
    pub last_model: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkDiffResult {
    pub original: TrajectoryStep,
    pub forked: TrajectoryStep,
    pub ttft_delta_ms: i64,
    pub output_tokens_delta: i64,
    pub cache_hit_ratio_delta: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkDiffExport {
    pub original_step_id: String,
    pub forked_step_id: String,
    pub original_model: String,
    pub forked_model: String,
    pub redacted: bool,
    pub redactions_applied: usize,
    pub sanitized_original_text: String,
    pub sanitized_forked_text: String,
    pub markdown: String,
}

fn build_unified_line_diff(old_text: &str, new_text: &str, max_lines: usize) -> String {
    let a: Vec<&str> = old_text.lines().collect();
    let b: Vec<&str> = new_text.lines().collect();
    let m = a.len();
    let n = b.len();

    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut lines = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m && j < n {
        if a[i] == b[j] {
            lines.push(format!("  {}", a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            lines.push(format!("- {}", a[i]));
            i += 1;
        } else {
            lines.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    while i < m {
        lines.push(format!("- {}", a[i]));
        i += 1;
    }
    while j < n {
        lines.push(format!("+ {}", b[j]));
        j += 1;
    }

    if lines.len() > max_lines {
        let omitted = lines.len() - max_lines;
        lines.truncate(max_lines);
        lines.push(format!("... ({} more diff lines omitted)", omitted));
    }
    lines.join("\n")
}

/// Generates a shareable Fork & Diff Markdown report comparing two trajectory steps,
/// optionally applying `GuardrailMiddleware` secret & PII redaction (recommended ON by default).
pub fn render_fork_diff_markdown(
    original: &TrajectoryStep,
    forked: &TrajectoryStep,
    redact_secrets: bool,
) -> ForkDiffExport {
    let (orig_text, c1) = if redact_secrets {
        redact_sensitive_text(&original.response_text, true, true)
    } else {
        (original.response_text.clone(), 0)
    };
    let (fork_text, c2) = if redact_secrets {
        redact_sensitive_text(&forked.response_text, true, true)
    } else {
        (forked.response_text.clone(), 0)
    };
    let total_redactions = c1 + c2;

    let ttft_delta = forked.ttft_ms as i64 - original.ttft_ms as i64;
    let out_delta = forked.output_tokens as i64 - original.output_tokens as i64;
    let orig_hit_pct = original.cache_hit_ratio() * 100.0;
    let fork_hit_pct = forked.cache_hit_ratio() * 100.0;
    let hit_delta_pp = fork_hit_pct - orig_hit_pct;
    let diff_block = build_unified_line_diff(&orig_text, &fork_text, 120);

    let privacy_banner = if redact_secrets {
        format!(
            "> Guardrail Redaction: **Enabled** ({} sensitive token(s)/PII scrubbed before export)\n",
            total_redactions
        )
    } else {
        "> Guardrail Redaction: **Disabled** (raw local output exported)\n".to_string()
    };

    let markdown = format!(
        "# LoomGate Fork & Diff Report\n\n\
         {privacy_banner}\n\
         - **Session**: `{session}` (Turn {turn})\n\
         - **Original Step**: `{orig_id}` → `{orig_model}` (`{orig_ch}`)\n\
         - **Forked Step**: `{fork_id}` → `{fork_model}` (`{fork_ch}`)\n\n\
         ## Performance & Token Comparison\n\n\
         | Metric | Original (`{orig_model}`) | Forked (`{fork_model}`) | Delta |\n\
         |---|---:|---:|---:|\n\
         | TTFT | {orig_ttft} ms | {fork_ttft} ms | {ttft_sign}{ttft_delta} ms |\n\
         | Input Tokens | {orig_in} | {fork_in} | {in_delta:+} |\n\
         | Cache Read Tokens | {orig_cache} | {fork_cache} | {cache_delta:+} |\n\
         | KV Cache Hit Rate | {orig_hit_pct:.1}% | {fork_hit_pct:.1}% | {hit_delta_pp:+.1} pp |\n\
         | Output Tokens | {orig_out} | {fork_out} | {out_sign}{out_delta} |\n\
         | Status | `{orig_status}` | `{fork_status}` | — |\n\n\
         ## Response Diff\n\n\
         ```diff\n\
         {diff_block}\n\
         ```\n",
        privacy_banner = privacy_banner,
        session = original.session_id,
        turn = original.turn_index,
        orig_id = original.step_id,
        orig_model = original.actual_model,
        orig_ch = original.channel_id,
        fork_id = forked.step_id,
        fork_model = forked.actual_model,
        fork_ch = forked.channel_id,
        orig_ttft = original.ttft_ms,
        fork_ttft = forked.ttft_ms,
        ttft_sign = if ttft_delta >= 0 { "+" } else { "" },
        ttft_delta = ttft_delta,
        orig_in = original.input_tokens,
        fork_in = forked.input_tokens,
        in_delta = forked.input_tokens as i64 - original.input_tokens as i64,
        orig_cache = original.cache_read_tokens,
        fork_cache = forked.cache_read_tokens,
        cache_delta = forked.cache_read_tokens as i64 - original.cache_read_tokens as i64,
        orig_hit_pct = orig_hit_pct,
        fork_hit_pct = fork_hit_pct,
        hit_delta_pp = hit_delta_pp,
        orig_out = original.output_tokens,
        fork_out = forked.output_tokens,
        out_sign = if out_delta >= 0 { "+" } else { "" },
        out_delta = out_delta,
        orig_status = original.status,
        fork_status = forked.status,
        diff_block = diff_block,
    );

    ForkDiffExport {
        original_step_id: original.step_id.clone(),
        forked_step_id: forked.step_id.clone(),
        original_model: original.actual_model.clone(),
        forked_model: forked.actual_model.clone(),
        redacted: redact_secrets,
        redactions_applied: total_redactions,
        sanitized_original_text: orig_text,
        sanitized_forked_text: fork_text,
        markdown,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSavingsRow {
    pub model: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub cache_hit_ratio: f64,
    pub aborted_count: u64,
    pub estimated_actual_cost_usd: f64,
    pub estimated_cache_savings_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavingsSummary {
    pub window_days: u32,
    pub total_requests: u64,
    pub completed_requests: u64,
    pub aborted_by_client_count: u64,
    pub sticky_hit_count: u64,
    pub total_input_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_output_tokens: u64,
    pub avg_cache_hit_ratio: f64,
    pub estimated_actual_cost_usd: f64,
    pub estimated_cache_savings_usd: f64,
    pub estimated_stop_loss_savings_usd: f64,
    pub by_model: Vec<ModelSavingsRow>,
}

/// Built-in conservative USD per-1M-token pricing table (overridable via `[model_pricing]` in TOML).
pub fn resolve_model_pricing(
    model: &str,
    overrides: &HashMap<String, ModelPricing>,
) -> ModelPricing {
    if let Some(p) = overrides.get(model) {
        return p.clone();
    }
    let lower = model.to_lowercase();
    if let Some((_, p)) = overrides
        .iter()
        .find(|(k, _)| lower.contains(&k.to_lowercase()))
    {
        return p.clone();
    }

    // Local self-hosted models (Ollama / LM Studio / vLLM conventions) cost $0/token
    if lower.contains(':')
        || lower.starts_with("llama")
        || lower.starts_with("qwen")
        || lower.starts_with("deepseek-r1:")
        || lower.starts_with("mistral")
    {
        return ModelPricing {
            input_per_1m_usd: 0.0,
            cache_read_per_1m_usd: 0.0,
            output_per_1m_usd: 0.0,
        };
    }

    if lower.contains("opus") {
        ModelPricing {
            input_per_1m_usd: 15.0,
            cache_read_per_1m_usd: 1.5,
            output_per_1m_usd: 75.0,
        }
    } else if lower.contains("sonnet") {
        ModelPricing {
            input_per_1m_usd: 3.0,
            cache_read_per_1m_usd: 0.3,
            output_per_1m_usd: 15.0,
        }
    } else if lower.contains("haiku") {
        ModelPricing {
            input_per_1m_usd: 0.8,
            cache_read_per_1m_usd: 0.08,
            output_per_1m_usd: 4.0,
        }
    } else if lower.contains("gpt-4o-mini") || lower.contains("o3-mini") {
        ModelPricing {
            input_per_1m_usd: 0.15,
            cache_read_per_1m_usd: 0.075,
            output_per_1m_usd: 0.6,
        }
    } else if lower.contains("gpt-4o") || lower.contains("gpt-4") {
        ModelPricing {
            input_per_1m_usd: 2.5,
            cache_read_per_1m_usd: 1.25,
            output_per_1m_usd: 10.0,
        }
    } else if lower.contains("gemini") && lower.contains("flash") {
        ModelPricing {
            input_per_1m_usd: 0.15,
            cache_read_per_1m_usd: 0.0375,
            output_per_1m_usd: 0.6,
        }
    } else if lower.contains("gemini") {
        ModelPricing {
            input_per_1m_usd: 1.25,
            cache_read_per_1m_usd: 0.3125,
            output_per_1m_usd: 10.0,
        }
    } else {
        ModelPricing {
            input_per_1m_usd: 2.0,
            cache_read_per_1m_usd: 0.2,
            output_per_1m_usd: 8.0,
        }
    }
}

/// Append-only local SQLite (WAL) store for Workspace -> Session -> Turn -> Step trajectories.
pub struct TrajectoryStore {
    conn: Mutex<Connection>,
}

impl TrajectoryStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TrajectoryError> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, TrajectoryError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, TrajectoryError> {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS trajectory_steps (
                step_id           TEXT PRIMARY KEY,
                workspace         TEXT NOT NULL,
                session_id        TEXT NOT NULL,
                parent_step_id    TEXT,
                turn_index        INTEGER NOT NULL,
                protocol          TEXT NOT NULL,
                channel_id        TEXT NOT NULL,
                requested_model   TEXT NOT NULL,
                actual_model      TEXT NOT NULL,
                route_mode        TEXT NOT NULL,
                sticky_hit        INTEGER NOT NULL,
                ttft_ms           INTEGER NOT NULL,
                input_tokens      INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL,
                output_tokens     INTEGER NOT NULL,
                status            TEXT NOT NULL,
                request_json      TEXT NOT NULL,
                response_text     TEXT NOT NULL,
                tool_calls_json   TEXT NOT NULL,
                created_at_ms     INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_traj_session_turn
                ON trajectory_steps(session_id, turn_index, created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_traj_parent
                ON trajectory_steps(parent_step_id);
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn next_turn_index(&self, session_id: &str) -> Result<i64, TrajectoryError> {
        let conn = self.conn.lock().expect("trajectory mutex poisoned");
        let max_turn: Option<i64> = conn.query_row(
            "SELECT MAX(turn_index) FROM trajectory_steps WHERE session_id = ?1 AND parent_step_id IS NULL",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(max_turn.unwrap_or(0) + 1)
    }

    /// Appends a new immutable step (or forked branch) to the event log.
    pub fn append_step(&self, step: &TrajectoryStep) -> Result<(), TrajectoryError> {
        let conn = self.conn.lock().expect("trajectory mutex poisoned");
        conn.execute(
            "
            INSERT INTO trajectory_steps (
                step_id, workspace, session_id, parent_step_id, turn_index,
                protocol, channel_id, requested_model, actual_model, route_mode,
                sticky_hit, ttft_ms, input_tokens, cache_read_tokens, output_tokens,
                status, request_json, response_text, tool_calls_json, created_at_ms
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20
            )
            ",
            params![
                step.step_id,
                step.workspace,
                step.session_id,
                step.parent_step_id,
                step.turn_index,
                step.protocol,
                step.channel_id,
                step.requested_model,
                step.actual_model,
                step.route_mode,
                if step.sticky_hit { 1 } else { 0 },
                step.ttft_ms as i64,
                step.input_tokens as i64,
                step.cache_read_tokens as i64,
                step.output_tokens as i64,
                step.status,
                step.request_json,
                step.response_text,
                step.tool_calls_json,
                step.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionSummary>, TrajectoryError> {
        let conn = self.conn.lock().expect("trajectory mutex poisoned");
        let mut stmt = conn.prepare(
            "
            SELECT
                session_id,
                MAX(workspace) AS workspace,
                COUNT(*) AS step_count,
                SUM(CASE WHEN parent_step_id IS NOT NULL THEN 1 ELSE 0 END) AS fork_count,
                SUM(cache_read_tokens) AS total_cache_read_tokens,
                SUM(input_tokens) AS total_input_tokens,
                (SELECT actual_model FROM trajectory_steps t2
                 WHERE t2.session_id = t1.session_id
                 ORDER BY t2.created_at_ms DESC LIMIT 1) AS last_model,
                MAX(created_at_ms) AS updated_at_ms
            FROM trajectory_steps t1
            GROUP BY session_id
            ORDER BY updated_at_ms DESC
            LIMIT ?1
            ",
        )?;

        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(SessionSummary {
                session_id: r.get(0)?,
                workspace: r.get(1)?,
                step_count: r.get(2)?,
                fork_count: r.get(3)?,
                total_cache_read_tokens: r.get(4)?,
                total_input_tokens: r.get(5)?,
                last_model: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                updated_at_ms: r.get(7)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_session_steps(&self, session_id: &str) -> Result<Vec<TrajectoryStep>, TrajectoryError> {
        let conn = self.conn.lock().expect("trajectory mutex poisoned");
        let mut stmt = conn.prepare(
            "
            SELECT
                step_id, workspace, session_id, parent_step_id, turn_index,
                protocol, channel_id, requested_model, actual_model, route_mode,
                sticky_hit, ttft_ms, input_tokens, cache_read_tokens, output_tokens,
                status, request_json, response_text, tool_calls_json, created_at_ms
            FROM trajectory_steps
            WHERE session_id = ?1
            ORDER BY turn_index ASC, created_at_ms ASC
            ",
        )?;

        let rows = stmt.query_map(params![session_id], map_step_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_step(&self, step_id: &str) -> Result<TrajectoryStep, TrajectoryError> {
        let conn = self.conn.lock().expect("trajectory mutex poisoned");
        conn.query_row(
            "
            SELECT
                step_id, workspace, session_id, parent_step_id, turn_index,
                protocol, channel_id, requested_model, actual_model, route_mode,
                sticky_hit, ttft_ms, input_tokens, cache_read_tokens, output_tokens,
                status, request_json, response_text, tool_calls_json, created_at_ms
            FROM trajectory_steps
            WHERE step_id = ?1
            ",
            params![step_id],
            map_step_row,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => TrajectoryError::NotFound(step_id.to_string()),
            other => TrajectoryError::Sqlite(other),
        })
    }

    /// Aggregates local SQLite WAL trajectories over the trailing `days` window (`0` = all time)
    /// to compute Prompt Caching hit rates, Ctrl+C stop-loss counts, and estimated USD savings.
    pub fn get_savings_summary(
        &self,
        days: u32,
        custom_pricing: &HashMap<String, ModelPricing>,
    ) -> Result<SavingsSummary, TrajectoryError> {
        let cutoff_ms: i64 = if days == 0 {
            0
        } else {
            now_epoch_ms().saturating_sub(i64::from(days) * 86_400_000)
        };

        let conn = self.conn.lock().expect("trajectory mutex poisoned");
        let mut stmt = conn.prepare(
            "
            SELECT
                actual_model,
                COUNT(*) AS reqs,
                SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END) AS completed_cnt,
                SUM(CASE WHEN status = 'aborted_by_client' THEN 1 ELSE 0 END) AS aborted_cnt,
                SUM(CASE WHEN sticky_hit != 0 THEN 1 ELSE 0 END) AS sticky_cnt,
                SUM(input_tokens) AS sum_in,
                SUM(cache_read_tokens) AS sum_cache,
                SUM(output_tokens) AS sum_out
            FROM trajectory_steps
            WHERE created_at_ms >= ?1
            GROUP BY actual_model
            ORDER BY reqs DESC, actual_model ASC
            ",
        )?;

        let rows = stmt.query_map(params![cutoff_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?.max(0) as u64,
                r.get::<_, i64>(2)?.max(0) as u64,
                r.get::<_, i64>(3)?.max(0) as u64,
                r.get::<_, i64>(4)?.max(0) as u64,
                r.get::<_, i64>(5)?.max(0) as u64,
                r.get::<_, i64>(6)?.max(0) as u64,
                r.get::<_, i64>(7)?.max(0) as u64,
            ))
        })?;

        let mut total_requests = 0u64;
        let mut completed_requests = 0u64;
        let mut aborted_by_client_count = 0u64;
        let mut sticky_hit_count = 0u64;
        let mut total_input_tokens = 0u64;
        let mut total_cache_read_tokens = 0u64;
        let mut total_output_tokens = 0u64;
        let mut estimated_actual_cost_usd = 0.0f64;
        let mut estimated_cache_savings_usd = 0.0f64;
        let mut estimated_stop_loss_savings_usd = 0.0f64;
        let mut by_model = Vec::new();

        for row in rows {
            let (model, reqs, comp, aborted, sticky, in_tok, cache_tok, out_tok) = row?;
            total_requests += reqs;
            completed_requests += comp;
            aborted_by_client_count += aborted;
            sticky_hit_count += sticky;
            total_input_tokens += in_tok;
            total_cache_read_tokens += cache_tok;
            total_output_tokens += out_tok;

            let price = resolve_model_pricing(&model, custom_pricing);
            let actual_cost = (in_tok as f64 * price.input_per_1m_usd
                + cache_tok as f64 * price.cache_read_per_1m_usd
                + out_tok as f64 * price.output_per_1m_usd)
                / 1_000_000.0;
            let cache_savings = (cache_tok as f64
                * (price.input_per_1m_usd - price.cache_read_per_1m_usd).max(0.0))
                / 1_000_000.0;
            // Conservative estimate: each mid-stream Ctrl+C abort avoids ~800 trailing output tokens
            let stop_loss_savings = (aborted as f64 * 800.0 * price.output_per_1m_usd) / 1_000_000.0;

            estimated_actual_cost_usd += actual_cost;
            estimated_cache_savings_usd += cache_savings;
            estimated_stop_loss_savings_usd += stop_loss_savings;

            let denom = in_tok + cache_tok;
            let hit_ratio = if denom == 0 {
                0.0
            } else {
                cache_tok as f64 / denom as f64
            };

            by_model.push(ModelSavingsRow {
                model,
                requests: reqs,
                input_tokens: in_tok,
                cache_read_tokens: cache_tok,
                output_tokens: out_tok,
                cache_hit_ratio: hit_ratio,
                aborted_count: aborted,
                estimated_actual_cost_usd: actual_cost,
                estimated_cache_savings_usd: cache_savings,
            });
        }

        let total_in_denom = total_input_tokens + total_cache_read_tokens;
        let avg_cache_hit_ratio = if total_in_denom == 0 {
            0.0
        } else {
            total_cache_read_tokens as f64 / total_in_denom as f64
        };

        Ok(SavingsSummary {
            window_days: days,
            total_requests,
            completed_requests,
            aborted_by_client_count,
            sticky_hit_count,
            total_input_tokens,
            total_cache_read_tokens,
            total_output_tokens,
            avg_cache_hit_ratio,
            estimated_actual_cost_usd,
            estimated_cache_savings_usd,
            estimated_stop_loss_savings_usd,
            by_model,
        })
    }
}

fn map_step_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrajectoryStep> {
    Ok(TrajectoryStep {
        step_id: r.get(0)?,
        workspace: r.get(1)?,
        session_id: r.get(2)?,
        parent_step_id: r.get(3)?,
        turn_index: r.get(4)?,
        protocol: r.get(5)?,
        channel_id: r.get(6)?,
        requested_model: r.get(7)?,
        actual_model: r.get(8)?,
        route_mode: r.get(9)?,
        sticky_hit: r.get::<_, i64>(10)? != 0,
        ttft_ms: r.get::<_, i64>(11)? as u64,
        input_tokens: r.get::<_, i64>(12)? as u64,
        cache_read_tokens: r.get::<_, i64>(13)? as u64,
        output_tokens: r.get::<_, i64>(14)? as u64,
        status: r.get(15)?,
        request_json: r.get(16)?,
        response_text: r.get(17)?,
        tool_calls_json: r.get(18)?,
        created_at_ms: r.get(19)?,
    })
}

pub fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_step_id() -> String {
    format!("step_{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_only_trajectory_and_fork_tree_queries() {
        let store = TrajectoryStore::open_in_memory().unwrap();
        let s1 = TrajectoryStep {
            step_id: "step_1".into(),
            workspace: "edge-gateway".into(),
            session_id: "sess_demo".into(),
            parent_step_id: None,
            turn_index: 1,
            protocol: "anthropic".into(),
            channel_id: "anthropic-primary".into(),
            requested_model: "claude-sonnet-4-20250514".into(),
            actual_model: "claude-sonnet-4-20250514".into(),
            route_mode: "lock".into(),
            sticky_hit: false,
            ttft_ms: 420,
            input_tokens: 1000,
            cache_read_tokens: 0,
            output_tokens: 180,
            status: "completed".into(),
            request_json: "{\"model\":\"claude-sonnet-4-20250514\",\"messages\":[]}".into(),
            response_text: "Original answer".into(),
            tool_calls_json: "[]".into(),
            created_at_ms: 1000,
        };
        store.append_step(&s1).unwrap();

        let fork = TrajectoryStep {
            step_id: "step_1_fork".into(),
            parent_step_id: Some("step_1".into()),
            actual_model: "gpt-4o".into(),
            channel_id: "openai-primary".into(),
            ttft_ms: 210,
            response_text: "Forked answer from gpt-4o".into(),
            created_at_ms: 2000,
            ..s1.clone()
        };
        store.append_step(&fork).unwrap();

        let sessions = store.list_sessions(10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].step_count, 2);
        assert_eq!(sessions[0].fork_count, 1);

        let steps = store.get_session_steps("sess_demo").unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].parent_step_id.as_deref(), Some("step_1"));
    }

    #[test]
    fn savings_summary_aggregates_cache_hits_aborts_and_custom_pricing() {
        let store = TrajectoryStore::open_in_memory().unwrap();
        let now = now_epoch_ms();
        let make = |id: &str, model: &str, in_t: u64, cache_t: u64, out_t: u64, status: &str| {
            TrajectoryStep {
                step_id: id.into(),
                workspace: "loomgate".into(),
                session_id: "s1".into(),
                parent_step_id: None,
                turn_index: 1,
                protocol: "anthropic".into(),
                channel_id: "anthropic-primary".into(),
                requested_model: model.into(),
                actual_model: model.into(),
                route_mode: "lock".into(),
                sticky_hit: cache_t > 0,
                ttft_ms: 120,
                input_tokens: in_t,
                cache_read_tokens: cache_t,
                output_tokens: out_t,
                status: status.into(),
                request_json: "{}".into(),
                response_text: "ok".into(),
                tool_calls_json: "[]".into(),
                created_at_ms: now,
            }
        };
        store
            .append_step(&make("st1", "claude-sonnet-4-20250514", 100_000, 900_000, 10_000, "completed"))
            .unwrap();
        store
            .append_step(&make("st2", "claude-sonnet-4-20250514", 50_000, 0, 200, "aborted_by_client"))
            .unwrap();

        let summary = store.get_savings_summary(30, &HashMap::new()).unwrap();
        assert_eq!(summary.total_requests, 2);
        assert_eq!(summary.aborted_by_client_count, 1);
        assert_eq!(summary.total_cache_read_tokens, 900_000);
        // 900k cached tokens on Sonnet saves 0.9 * ($3.00 - $0.30) = $2.43
        assert!((summary.estimated_cache_savings_usd - 2.43).abs() < 1e-4);
        assert!(summary.estimated_stop_loss_savings_usd > 0.0);
        assert_eq!(summary.by_model.len(), 1);
    }

    #[test]
    fn export_fork_diff_markdown_redacts_sk_and_bearer_tokens_by_default() {
        let orig = TrajectoryStep {
            step_id: "step_orig".into(),
            workspace: "loomgate".into(),
            session_id: "sess_export".into(),
            parent_step_id: None,
            turn_index: 2,
            protocol: "anthropic".into(),
            channel_id: "anthropic-primary".into(),
            requested_model: "claude-sonnet-4-20250514".into(),
            actual_model: "claude-sonnet-4-20250514".into(),
            route_mode: "lock".into(),
            sticky_hit: true,
            ttft_ms: 320,
            input_tokens: 200,
            cache_read_tokens: 1800,
            output_tokens: 45,
            status: "completed".into(),
            request_json: "{}".into(),
            response_text: "Export token sk-ant-supersecret12345678\nAuthorization: Bearer tok_live_abc123456789".into(),
            tool_calls_json: "[]".into(),
            created_at_ms: 1000,
        };
        let forked = TrajectoryStep {
            step_id: "step_fork".into(),
            parent_step_id: Some("step_orig".into()),
            actual_model: "gpt-4o".into(),
            channel_id: "openai-primary".into(),
            ttft_ms: 190,
            output_tokens: 38,
            response_text: "Safe reply without key\nContact ops@loomgate.dev".into(),
            ..orig.clone()
        };

        let exported = render_fork_diff_markdown(&orig, &forked, true);
        assert!(exported.redacted);
        assert!(exported.redactions_applied >= 3);
        assert!(!exported.markdown.contains("sk-ant-supersecret12345678"));
        assert!(!exported.markdown.contains("tok_live_abc123456789"));
        assert!(!exported.markdown.contains("ops@loomgate.dev"));
        assert!(exported.markdown.contains("[REDACTED_SECRET]"));
        assert!(exported.markdown.contains("[REDACTED_EMAIL]"));
        assert!(exported.markdown.contains("## Performance & Token Comparison"));
        assert!(exported.markdown.contains("```diff"));
    }
}
