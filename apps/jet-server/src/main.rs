use std::{
    collections::HashMap, net::SocketAddr, path::Path, sync::Arc, sync::atomic::AtomicU64,
    time::Instant,
};

use apalis_redis::RedisStorage;
use deadpool_redis::{Config as PoolConfig, Runtime as PoolRuntime, redis::AsyncCommands};
use jet_core::JetConfig;
use jet_pack::{RedisVersionStore, VersionResolver};
use tracing::{error, info, warn};

mod api;
mod queue;
mod sandbox;
mod worker;

/// The namespace apalis-redis uses for our QueuedJob type.
/// apalis defaults to `std::any::type_name::<T>()`.
const APALIS_QUEUE_NAMESPACE: &str = "jet_server::queue::QueuedJob";

/// All the key suffixes apalis-redis creates under the namespace.
const APALIS_KEY_SUFFIXES: &[&str] = &[
    "active",
    "consumers",
    "dead",
    "done",
    "failed",
    "inflight",
    "data",
    "scheduled",
    "signal",
];

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
    let all_manifests = resolver.initialize_from_manifest_dir(&config.runtimes_manifest_dir)?;

    // Filter manifests to only include runtimes that are actually installed on disk.
    let total_manifest_count = all_manifests.len();
    let mut installed_manifests = Vec::new();
    for manifest in &all_manifests {
        let runtime_root = config
            .runtime_install_dir
            .join(&manifest.language)
            .join(&manifest.version)
            .join("root");
        if runtime_root.exists() {
            installed_manifests.push(manifest.clone());
        } else {
            warn!(
                language = %manifest.language,
                version = %manifest.version,
                expected_path = %runtime_root.display(),
                "runtime not installed, skipping"
            );
        }
    }

    // Rebuild the version map with only installed runtimes so that uninstalled
    // versions are rejected immediately at the API level.
    resolver.initialize_from_manifests(&installed_manifests)?;

    let mut manifest_map = HashMap::new();
    for manifest in installed_manifests {
        manifest_map.insert(
            format!("{}:{}", manifest.language, manifest.version),
            manifest,
        );
    }
    let manifest_count = manifest_map.len();

    if manifest_count < total_manifest_count {
        warn!(
            installed = manifest_count,
            total = total_manifest_count,
            skipped = total_manifest_count - manifest_count,
            "some runtimes declared in manifests are not installed"
        );
    }

    // Flush stale jobs from previous server runs before starting the worker.
    let mut flush_conn = apalis_redis::connect(config.redis_url.clone()).await?;
    let flushed = flush_stale_queue(&mut flush_conn).await;
    match flushed {
        Ok(count) if count > 0 => {
            info!(
                keys_removed = count,
                "flushed stale queue from previous run"
            );
        }
        Ok(_) => {}
        Err(err) => {
            warn!(error = %err, "failed to flush stale queue (non-fatal, continuing)");
        }
    }

    // Clean up stale workspace directories left over from previous runs.
    let jobs_dir = config.runtime_install_dir.join("jobs");
    cleanup_stale_workspaces(&jobs_dir).await;

    // Create a deadpool-redis connection pool shared by API + worker.
    let pool_cfg = PoolConfig::from_url(&config.redis_url);
    let redis_pool = pool_cfg.create_pool(Some(PoolRuntime::Tokio1))?;

    let conn = apalis_redis::connect(config.redis_url.clone()).await?;
    let storage = RedisStorage::<queue::QueuedJob>::new(conn);
    let job_state_prefix = "jet:jobs".to_string();

    let jobs_submitted = Arc::new(AtomicU64::new(0));
    let jobs_completed = Arc::new(AtomicU64::new(0));
    let jobs_failed = Arc::new(AtomicU64::new(0));
    let jobs_in_flight = Arc::new(AtomicU64::new(0));

    // Determine worker concurrency: defaults to CPU core count.
    let worker_concurrency = std::env::var("JET_WORKER_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });

    // Maximum queue depth before rejecting new submissions (backpressure).
    let max_queue_depth = std::env::var("JET_MAX_QUEUE_DEPTH")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(|| (worker_concurrency as u64) * 10);

    let api_state = api::ApiState {
        resolver: Arc::new(resolver),
        manifests: Arc::new(manifest_map.clone()),
        storage: Arc::new(tokio::sync::Mutex::new(storage)),
        redis_pool: redis_pool.clone(),
        job_state_prefix: job_state_prefix.clone(),
        start_time: Instant::now(),
        jobs_submitted: jobs_submitted.clone(),
        jobs_completed: jobs_completed.clone(),
        jobs_failed: jobs_failed.clone(),
        worker_concurrency,
        jobs_in_flight: jobs_in_flight.clone(),
        max_queue_depth,
    };

    let worker_context = worker::runner::WorkerContext {
        manifests: Arc::new(manifest_map),
        runtime_install_dir: config.runtime_install_dir.clone(),
        redis_pool,
        job_state_prefix,
        jobs_completed,
        jobs_failed,
        jobs_in_flight,
    };

    let worker_redis_url = config.redis_url.clone();
    let worker_handle = tokio::spawn(async move {
        if let Err(err) = worker::runner::run_worker(
            "jet-worker".to_string(),
            worker_redis_url,
            worker_context,
            worker_concurrency,
        )
        .await
        {
            error!(error = %err, "worker stopped with error");
        }
    });

    let app = api::router(api_state).layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024));
    let addr = format!("{}:{}", config.server_host, config.server_port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!(
        installed = manifest_count,
        total_manifests = total_manifest_count,
        worker_concurrency = worker_concurrency,
        cache_key = %config.runtime_cache_key,
        "startup complete: loaded installed runtimes, version map cached"
    );
    info!(addr = %addr, "server running");

    // Use into_make_service_with_connect_info so tower-governor can
    // extract the client IP for per-IP rate limiting.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // After the HTTP server shuts down, stop the worker.
    info!("stopping worker...");
    worker_handle.abort();
    match worker_handle.await {
        Ok(_) => info!("worker stopped cleanly"),
        Err(e) if e.is_cancelled() => info!("worker stopped"),
        Err(e) => error!(error = %e, "worker stopped with error"),
    }

    info!("shutdown complete");
    Ok(())
}

/// Flush all stale apalis queue keys from Redis.
///
/// This prevents old jobs from previous server runs from being re-processed
/// on startup (which would fail with "manifest missing" errors).
async fn flush_stale_queue(
    conn: &mut apalis_redis::ConnectionManager,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut removed = 0usize;
    for suffix in APALIS_KEY_SUFFIXES {
        let key = format!("{}:{}", APALIS_QUEUE_NAMESPACE, suffix);
        let deleted: usize = conn.del(&key).await.unwrap_or(0);
        removed += deleted;
    }
    Ok(removed)
}

/// Clean up orphaned job workspace directories from previous server runs
/// that may have crashed before completing cleanup.
async fn cleanup_stale_workspaces(jobs_dir: &Path) {
    let mut entries = match tokio::fs::read_dir(jobs_dir).await {
        Ok(entries) => entries,
        Err(_) => return, // Directory doesn't exist, nothing to clean
    };

    let mut removed = 0u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.path().is_dir() {
            if let Err(e) = tokio::fs::remove_dir_all(entry.path()).await {
                warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "failed to clean stale workspace"
                );
            } else {
                removed += 1;
            }
        }
    }

    if removed > 0 {
        info!(
            removed = removed,
            "cleaned stale job workspaces from previous run"
        );
    }
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
