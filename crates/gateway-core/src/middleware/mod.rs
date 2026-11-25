pub mod guardrail;
pub mod ingress;
pub mod mcp_bridge;
pub mod memory;

use crate::config::UpstreamProtocol;
use crate::ir::{CanonicalRequest, CanonicalResponse};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Structured short-circuit rejection returned by any middleware in the onion pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiddlewareRejection {
    pub status: u16,
    pub error_type: String,
    pub message: String,
    pub middleware: &'static str,
}

impl MiddlewareRejection {
    pub fn new(
        middleware: &'static str,
        status: u16,
        error_type: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            error_type: error_type.into(),
            message: message.into(),
            middleware,
        }
    }
}

/// Mutable per-request context passed through the onion middleware pipeline.
/// Preserves lossless `CanonicalRequest` on the inbound leg and `CanonicalResponse` on the outbound leg.
#[derive(Debug, Clone)]
pub struct MiddlewareContext {
    pub request_id: String,
    pub downstream_protocol: UpstreamProtocol,
    pub client_key: String,
    pub workspace: String,
    pub request: CanonicalRequest,
    /// Populated by the terminal handler on non-streaming responses so post-legs can inspect/guard them.
    pub response: Option<CanonicalResponse>,
    /// Transparent observability headers emitted by middlewares (e.g. X-Gateway-Guardrail-Redactions).
    pub annotations: BTreeMap<String, String>,
}

impl MiddlewareContext {
    pub fn new(
        request_id: String,
        downstream_protocol: UpstreamProtocol,
        client_key: String,
        workspace: String,
        request: CanonicalRequest,
    ) -> Self {
        Self {
            request_id,
            downstream_protocol,
            client_key,
            workspace,
            request,
            response: None,
            annotations: BTreeMap::new(),
        }
    }
}

pub type NextFn<'a> = Box<
    dyn FnOnce(MiddlewareContext) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>>
        + Send
        + 'a,
>;

type TerminalFn<'a, T, E> =
    Box<dyn FnOnce(MiddlewareContext) -> BoxFuture<'a, (MiddlewareContext, Result<T, E>)> + Send + 'a>;

/// Onion-model Middleware Trait for the LoomGate microkernel.
///
/// Implementations run inbound logic on `ctx` BEFORE calling `next(ctx).await?`, then receive
/// `ctx` back on the return leg for outbound post-processing.
pub trait Middleware: Send + Sync {
    fn name(&self) -> &'static str;

    fn handle<'a>(
        &'a self,
        ctx: MiddlewareContext,
        next: NextFn<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>>;
}

/// Ordered onion pipeline executing `M_0 -> M_1 -> ... -> terminal -> ... -> M_1 -> M_0`.
#[derive(Default, Clone)]
pub struct MiddlewarePipeline {
    layers: Vec<Arc<dyn Middleware>>,
}

impl MiddlewarePipeline {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    pub fn push(&mut self, middleware: Arc<dyn Middleware>) {
        self.layers.push(middleware);
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    pub fn layer_names(&self) -> Vec<&'static str> {
        self.layers.iter().map(|m| m.name()).collect()
    }

    /// Executes the full onion pipeline around `terminal`.
    /// Returns:
    /// - `Err(MiddlewareRejection)` if any pre-leg or post-leg middleware short-circuits/blocks;
    /// - `Ok((final_ctx, terminal_result))` otherwise.
    pub async fn execute<'a, T, E, F>(
        &'a self,
        ctx: MiddlewareContext,
        terminal: F,
    ) -> Result<(MiddlewareContext, Result<T, E>), MiddlewareRejection>
    where
        T: Send + 'a,
        E: Send + 'a,
        F: FnOnce(MiddlewareContext) -> BoxFuture<'a, (MiddlewareContext, Result<T, E>)> + Send + 'a,
    {
        let output: Arc<Mutex<Option<Result<T, E>>>> = Arc::new(Mutex::new(None));
        let terminal_box: TerminalFn<'a, T, E> = Box::new(terminal);
        let final_ctx = run_layer(&self.layers, ctx, terminal_box, Arc::clone(&output)).await?;
        let terminal_res = output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("middleware pipeline must invoke terminal unless rejected");
        Ok((final_ctx, terminal_res))
    }
}

fn run_layer<'a, T: Send + 'a, E: Send + 'a>(
    layers: &'a [Arc<dyn Middleware>],
    ctx: MiddlewareContext,
    terminal: TerminalFn<'a, T, E>,
    output: Arc<Mutex<Option<Result<T, E>>>>,
) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>> {
    Box::pin(async move {
        if let Some((first, rest)) = layers.split_first() {
            let next_cb: NextFn<'a> = Box::new(move |c: MiddlewareContext| {
                run_layer(rest, c, terminal, output)
            });
            first.handle(ctx, next_cb).await
        } else {
            let (returned_ctx, res) = terminal(ctx).await;
            *output.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
            Ok(returned_ctx)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CanonicalMessage, ContentPart, Role};

    struct TraceMiddleware {
        tag: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Middleware for TraceMiddleware {
        fn name(&self) -> &'static str {
            self.tag
        }

        fn handle<'a>(
            &'a self,
            ctx: MiddlewareContext,
            next: NextFn<'a>,
        ) -> BoxFuture<'a, Result<MiddlewareContext, MiddlewareRejection>> {
            Box::pin(async move {
                self.log.lock().unwrap().push(format!("{}:pre", self.tag));
                let ctx = next(ctx).await?;
                self.log.lock().unwrap().push(format!("{}:post", self.tag));
                Ok(ctx)
            })
        }
    }

    fn dummy_ctx() -> MiddlewareContext {
        let req = CanonicalRequest {
            model: "claude-sonnet-4-20250514".into(),
            system: vec![],
            messages: vec![CanonicalMessage {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: "hello".into(),
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
            session_id: Some("s1".into()),
            task_stage: None,
            extra_fields: Default::default(),
        };
        MiddlewareContext::new(
            "req_1".into(),
            UpstreamProtocol::Anthropic,
            "s1".into(),
            "default".into(),
            req,
        )
    }

    #[tokio::test]
    async fn onion_pipeline_executes_symmetric_pre_and_post_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut pipe = MiddlewarePipeline::new();
        pipe.push(Arc::new(TraceMiddleware {
            tag: "ingress",
            log: Arc::clone(&log),
        }));
        pipe.push(Arc::new(TraceMiddleware {
            tag: "guardrail",
            log: Arc::clone(&log),
        }));
        pipe.push(Arc::new(TraceMiddleware {
            tag: "memory",
            log: Arc::clone(&log),
        }));

        let ctx = dummy_ctx();
        let term_log = Arc::clone(&log);
        let (_final_ctx, res) = pipe
            .execute(ctx, move |c| {
                Box::pin(async move {
                    term_log.lock().unwrap().push("terminal".into());
                    (c, Ok::<_, ()>(42usize))
                })
            })
            .await
            .expect("pipeline should succeed");

        assert_eq!(res.unwrap(), 42);
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "ingress:pre",
                "guardrail:pre",
                "memory:pre",
                "terminal",
                "memory:post",
                "guardrail:post",
                "ingress:post"
            ]
        );
    }
}
