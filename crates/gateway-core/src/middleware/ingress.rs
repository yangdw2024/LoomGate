use crate::middleware::{BoxFuture, Middleware, MiddlewareContext, MiddlewareRejection, NextFn};
use dashmap::DashMap;
use std::time::Instant;

#[derive(Debug, Clone)]
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

/// Ingress rate-limiting middleware (lock-free per-client token bucket).
/// Setting `rpm_limit = 0` disables rate limiting while preserving the pipeline slot.
pub struct IngressRateLimitMiddleware {
    rpm_limit: u32,
    burst: f64,
    buckets: DashMap<String, TokenBucket>,
}

impl IngressRateLimitMiddleware {
    pub fn new(rpm_limit: u32, burst: u32) -> Self {
        let effective_burst = if burst == 0 {
            rpm_limit.max(1) as f64
        } else {
            burst as f64
        };
        Self {
            rpm_limit,
            burst: effective_burst,
            buckets: DashMap::new(),
        }
    }

    pub fn reset(&self) {
        self.buckets.clear();
    }

    fn try_acquire(&self, key: &str) -> Result<u32, ()> {
        if self.rpm_limit == 0 {
            return Ok(u32::MAX);
        }
        let now = Instant::now();
        let refill_per_sec = f64::from(self.rpm_limit) / 60.0;

        let mut entry = self.buckets.entry(key.to_string()).or_insert(TokenBucket {
            tokens: self.burst,
            last_refill: now,
        });

        let elapsed = now.duration_since(entry.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            entry.tokens = (entry.tokens + elapsed * refill_per_sec).min(self.burst);
            entry.last_refill = now;
        }

        if entry.tokens >= 1.0 {
            entry.tokens -= 1.0;
            Ok(entry.tokens.floor() as u32)
        } else {
            Err(())
        }
    }
}

impl Middleware for IngressRateLimitMiddleware {
    fn name(&self) -> &'static str {
        "ingress_rate_limit"
    }

    fn handle<'a>(
        &'a self,
        mut ctx: MiddlewareContext,
        next: NextFn<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>> {
        Box::pin(async move {
            if self.rpm_limit > 0 {
                match self.try_acquire(&ctx.client_key) {
                    Ok(remaining) => {
                        ctx.annotations
                            .insert("X-Gateway-RateLimit-Remaining".into(), remaining.to_string());
                    }
                    Err(()) => {
                        return Err(MiddlewareRejection::new(
                            self.name(),
                            429,
                            "rate_limit_error",
                            format!(
                                "local ingress rate limit exceeded ({} req/min) for client '{}'",
                                self.rpm_limit, ctx.client_key
                            ),
                        ));
                    }
                }
            }
            next(ctx).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamProtocol;
    use crate::ir::{CanonicalMessage, CanonicalRequest, ContentPart, Role};
    use crate::middleware::MiddlewarePipeline;
    use std::sync::Arc;

    #[tokio::test]
    async fn token_bucket_enforces_burst_limit() {
        let mut pipe = MiddlewarePipeline::new();
        pipe.push(Arc::new(IngressRateLimitMiddleware::new(60, 2)));

        let make_ctx = || {
            let req = CanonicalRequest {
                model: "gpt-4o".into(),
                system: vec![],
                messages: vec![CanonicalMessage {
                    role: Role::User,
                    parts: vec![ContentPart::Text {
                        text: "ping".into(),
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
                session_id: Some("rl-client".into()),
                task_stage: None,
                extra_fields: Default::default(),
            };
            MiddlewareContext::new(
                "r".into(),
                UpstreamProtocol::OpenAi,
                "rl-client".into(),
                "default".into(),
                req,
            )
        };

        for _ in 0..2 {
            let ctx = make_ctx();
            let out = pipe
                .execute(ctx, |c| Box::pin(async move { (c, Ok::<_, ()>(())) }))
                .await;
            assert!(out.is_ok());
        }

        let ctx3 = make_ctx();
        let err = pipe
            .execute(ctx3, |c| Box::pin(async move { (c, Ok::<_, ()>(())) }))
            .await
            .unwrap_err();
        assert_eq!(err.status, 429);
        assert_eq!(err.error_type, "rate_limit_error");
    }
}
