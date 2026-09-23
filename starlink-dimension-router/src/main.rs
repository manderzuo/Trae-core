use std::{fs, net::SocketAddr, path::PathBuf};

use starlink_dimension_router::{bridge_client::BridgeClient, config::RouterConfig, server::build_router, state::StarlinkRouterState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = RouterConfig::load(PathBuf::from(r"D:\gpt\starlink-dimension-router-data"))
        .map_err(std::io::Error::other)?;
    fs::create_dir_all(&config.data_dir)?;
    let (base_url, key) = config
        .bridge
        .as_ref()
        .map(|bridge| {
            let secret = fs::read_to_string(config.data_dir.join("bridge_secret.txt")).unwrap_or_default();
            (bridge.base_url.clone(), secret.trim().to_string())
        })
        .unwrap_or_else(|| (String::new(), String::new()));
    let state = StarlinkRouterState::open(config.clone(), BridgeClient::new(base_url, key))?;
    let app = build_router(state);
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    println!("{} listening on http://{}", config.display_name, addr);
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app)
        .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
        .await?;
    Ok(())
}
