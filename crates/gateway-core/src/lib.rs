pub mod cli_takeover;
pub mod cloud_sync;
pub mod codec;
pub mod config;
pub mod engine;
pub mod error_gov;
pub mod ir;
pub mod middleware;
pub mod router;
pub mod signature_cache;
pub mod sse_fsm;
pub mod trajectory;

pub use cli_takeover::{CliIntegrationStatus, CliTakeoverManager, CliTool, TakeoverError};
pub use cloud_sync::{CloudSessionStatus, CloudSyncClient, SanitizedTelemetryUpload};
pub use config::{ChannelConfig, GatewayConfig, MiddlewareConfig, RouteMode, UpstreamProtocol};
pub use engine::{GatewayEngine, GatewayError, GatewayResponse, ResponseMetadata};
pub use error_gov::ErrorTier;
pub use ir::{CanonicalRequest, CanonicalStreamEvent, CanonicalUsage};
pub use middleware::{
    mcp_bridge::McpServerConfig, Middleware, MiddlewareContext, MiddlewarePipeline,
    MiddlewareRejection,
};
pub use router::{ChannelHealth, ChannelSnapshot};
pub use trajectory::{ForkDiffResult, SessionSummary, TrajectoryError, TrajectoryStep, TrajectoryStore};
