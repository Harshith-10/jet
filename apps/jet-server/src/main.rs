use jet_core::JetConfig;
use jet_pack::{RedisVersionStore, VersionResolver};

mod sandbox;
mod worker;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = JetConfig::load()?;

    let redis_store = RedisVersionStore::new(&config.redis_url, &config.runtime_cache_key)?;
    let mut resolver = VersionResolver::new(redis_store);
    let manifests = resolver.initialize_from_manifest_dir(&config.runtimes_manifest_dir)?;

    println!(
        "jet-server startup complete: loaded {} manifest(s), version map cached at key '{}'",
        manifests.len(),
        config.runtime_cache_key
    );
    println!(
        "server placeholder running on {}:{}",
        config.server_host, config.server_port
    );

    Ok(())
}
