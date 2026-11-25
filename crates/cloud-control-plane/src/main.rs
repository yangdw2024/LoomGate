use cloud_control_plane::{build_router, ControlPlaneState};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cloud_control_plane=info".into()),
        )
        .init();

    let bind_addr = std::env::var("CLOUD_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8790".into());
    let state = ControlPlaneState::with_default_seed();
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!(bind_addr = %bind_addr, "LoomGate Cloud Control Plane listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("Cloud Control Plane shutting down cleanly");
        })
        .await?;
    Ok(())
}
