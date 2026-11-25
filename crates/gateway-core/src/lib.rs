pub mod cli_takeover;
pub mod codec;
pub mod config;
pub mod engine;
pub mod error_gov;
pub mod ir;
pub mod router;
pub mod signature_cache;
pub mod sse_fsm;
pub mod trajectory;

pub use cli_takeover::{CliIntegrationStatus, CliTakeoverManager, CliTool, TakeoverError};
pub use config::{ChannelConfig, GatewayConfig, RouteMode, UpstreamProtocol};
pub use engine::{GatewayEngine, GatewayError, GatewayResponse, ResponseMetadata};
pub use error_gov::ErrorTier;
pub use ir::{CanonicalRequest, CanonicalStreamEvent, CanonicalUsage};
pub use router::{ChannelHealth, ChannelSnapshot};
pub use trajectory::{ForkDiffResult, SessionSummary, TrajectoryError, TrajectoryStep, TrajectoryStore};
