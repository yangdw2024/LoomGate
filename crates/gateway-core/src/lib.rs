pub mod cli_takeover;
pub mod cloud_sync;
pub mod codec;
pub mod config;
pub mod engine;
pub mod error_gov;
pub mod ir;
pub mod local_discovery;
pub mod middleware;
pub mod router;
pub mod signature_cache;
pub mod sse_fsm;
pub mod trajectory;

pub use cli_takeover::{
    AtomicTakeoverSpec, CliIntegrationStatus, CliTakeoverManager, CliTool, ClientProfile,
    ClientProfileStatus, ConfigFormat, IntegrationKind, SetupWizardSpec, TakeoverError,
};
pub use cloud_sync::{CloudSessionStatus, CloudSyncClient, SanitizedTelemetryUpload};
pub use config::{
    normalize_base_url, ChannelConfig, GatewayConfig, MiddlewareConfig, ModelPricing, RouteMode,
    ServerConfig, UpstreamProtocol,
};
pub use engine::{
    AuthSettings, ChannelTestResult, GatewayEngine, GatewayError, GatewayResponse, ResponseMetadata,
};
pub use error_gov::ErrorTier;
pub use ir::{CanonicalRequest, CanonicalStreamEvent, CanonicalUsage};
pub use local_discovery::{
    build_local_channel_config, discover_local_providers, DiscoveredLocalProvider, LocalProbeSpec,
    LocalProviderKind,
};
pub use middleware::{
    mcp_bridge::McpServerConfig, Middleware, MiddlewareContext, MiddlewarePipeline,
    MiddlewareRejection,
};
pub use router::{ChannelHealth, ChannelSnapshot};
pub use trajectory::{
    ForkDiffResult, ModelSavingsRow, SavingsSummary, SessionSummary, TrajectoryError,
    TrajectoryStep, TrajectoryStore,
};
