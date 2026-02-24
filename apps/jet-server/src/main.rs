use std::{collections::HashMap, sync::Arc};

use apalis_redis::RedisStorage;
use jet_core::JetConfig;
use jet_pack::{RedisVersionStore, VersionResolver};
use tokio::sync::Mutex;
use tracing::{error, info};

mod api;
mod queue;
mod sandbox;
mod worker;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = JetConfig::load()?;

    let redis_store = RedisVersionStore::new(&config.redis_url, &config.runtime_cache_key)?;
    let mut resolver = VersionResolver::new(redis_store);
    let manifests = resolver.initialize_from_manifest_dir(&config.runtimes_manifest_dir)?;

    let mut manifest_map = HashMap::new();
    for manifest in manifests {
        manifest_map.insert(
            format!("{}:{}", manifest.language, manifest.version),
            manifest,
        );
    }
    let manifest_count = manifest_map.len();

    let conn = apalis_redis::connect(config.redis_url.clone()).await?;
    let storage = RedisStorage::<queue::QueuedJob>::new(conn);
    let redis_conn = Arc::new(Mutex::new(
        apalis_redis::connect(config.redis_url.clone()).await?,
    ));
    let job_state_prefix = "jet:jobs".to_string();

    let api_state = api::ApiState {
        resolver: Arc::new(resolver),
        manifests: Arc::new(manifest_map.clone()),
        storage: Arc::new(Mutex::new(storage)),
        redis_conn: redis_conn.clone(),
        job_state_prefix: job_state_prefix.clone(),
    };

    let worker_context = worker::runner::WorkerContext {
        manifests: Arc::new(manifest_map),
        runtime_install_dir: config.runtime_install_dir.clone(),
        redis_conn,
        job_state_prefix,
    };

    let worker_redis_url = config.redis_url.clone();
    tokio::spawn(async move {
        if let Err(err) = worker::runner::run_worker(worker_redis_url, worker_context).await {
            error!(error = %err, "worker stopped with error");
        }
    });

    let app = api::router(api_state).layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024));
    let addr = format!("{}:{}", config.server_host, config.server_port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!(
        manifests = manifest_count,
        cache_key = %config.runtime_cache_key,
        "startup complete: loaded manifests, version map cached"
    );
    info!(addr = %addr, "server running");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("signal received: ctrl-c");
        },
        _ = terminate => {
            info!("signal received: terminate");
        },
    }

    info!("graceful shutdown initiated");
}
