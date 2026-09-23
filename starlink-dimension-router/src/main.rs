use std::{fs, net::SocketAddr};

use starlink_dimension_router::{bridge_client::BridgeClient, config::RouterConfig, server::build_router, state::StarlinkRouterState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable_dir = std::env::current_exe()?
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or(std::env::current_dir()?);
    let config = RouterConfig::load(executable_dir.join("data"))
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
