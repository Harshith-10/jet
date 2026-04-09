use std::{env, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{JetError, JetResult};

const DEFAULT_SERVER_HOST: &str = "0.0.0.0";
const DEFAULT_SERVER_PORT: u16 = 4000;
const DEFAULT_REDIS_URL: &str = "redis://127.0.0.1:6379";
const DEFAULT_RUNTIME_CACHE_KEY: &str = "jet:version_map";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JetConfig {
    pub server_host: String,
    pub server_port: u16,
    pub redis_url: String,
    pub runtime_install_dir: PathBuf,
    pub runtimes_manifest_dir: PathBuf,
    pub runtime_cache_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    pub server_host: Option<String>,
    pub server_port: Option<u16>,
    pub redis_url: Option<String>,
    pub runtime_install_dir: Option<PathBuf>,
    pub runtimes_manifest_dir: Option<PathBuf>,
    pub runtime_cache_key: Option<String>,
}

impl JetConfig {
    pub fn load() -> JetResult<Self> {
        let mut config = if let Ok(path) = env::var("JET_CONFIG_PATH") {
            Self::from_file(path)?
        } else {
            Self::default_values()?
        };

        config.apply_env_overrides()?;
        Ok(config)
    }

    pub fn from_file(path: impl Into<PathBuf>) -> JetResult<Self> {
        let path = path.into();
        let raw = fs::read_to_string(&path).map_err(|source| JetError::Io {
            path: path.clone(),
            source,
        })?;

        let file: ConfigFile = toml::from_str(&raw).map_err(|source| JetError::ConfigParse {
            path: path.clone(),
            source,
        })?;

        Self::from_partial(file)
    }

    fn from_partial(file: ConfigFile) -> JetResult<Self> {
        let default = Self::default_values()?;

        Ok(Self {
            server_host: file.server_host.unwrap_or(default.server_host),
            server_port: file.server_port.unwrap_or(default.server_port),
            redis_url: file.redis_url.unwrap_or(default.redis_url),
            runtime_install_dir: file
                .runtime_install_dir
                .unwrap_or(default.runtime_install_dir),
            runtimes_manifest_dir: file
                .runtimes_manifest_dir
                .unwrap_or(default.runtimes_manifest_dir),
            runtime_cache_key: file.runtime_cache_key.unwrap_or(default.runtime_cache_key),
        })
    }

    fn default_values() -> JetResult<Self> {
        let runtime_install_dir = resolve_runtime_install_dir()?;
        let runtimes_manifest_dir = runtime_install_dir.join("manifests");

        Ok(Self {
            server_host: DEFAULT_SERVER_HOST.to_string(),
            server_port: DEFAULT_SERVER_PORT,
            redis_url: DEFAULT_REDIS_URL.to_string(),
            runtime_install_dir,
            runtimes_manifest_dir,
            runtime_cache_key: DEFAULT_RUNTIME_CACHE_KEY.to_string(),
        })
    }

    fn apply_env_overrides(&mut self) -> JetResult<()> {
        if let Ok(host) = env::var("JET_SERVER_HOST") {
            self.server_host = host;
        }

        if let Ok(port) = env::var("JET_SERVER_PORT") {
            self.server_port = port
                .parse::<u16>()
                .map_err(|_| JetError::config("JET_SERVER_PORT must be a valid u16"))?;
        }

        if let Ok(redis_url) = env::var("JET_REDIS_URL") {
            self.redis_url = redis_url;
        }

        if let Ok(dir) = env::var("JET_RUNTIME_DIR") {
            self.runtime_install_dir = PathBuf::from(dir);
        }

        if let Ok(dir) = env::var("JET_RUNTIME_MANIFEST_DIR") {
            self.runtimes_manifest_dir = PathBuf::from(dir);
        } else {
            self.runtimes_manifest_dir = self.runtime_install_dir.join("manifests");
        }

        if let Ok(cache_key) = env::var("JET_RUNTIME_CACHE_KEY") {
            self.runtime_cache_key = cache_key;
        }

        Ok(())
    }
}

pub fn resolve_runtime_install_dir() -> JetResult<PathBuf> {
    if let Ok(path) = env::var("JET_RUNTIME_DIR") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(home) = env::var("HOME") {
        return Ok(PathBuf::from(home).join(".jet").join("runtimes"));
    }

    Ok(PathBuf::from("/var/lib/jet/runtimes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;

    use tempfile::NamedTempFile;

    fn clear_phase_one_env() {
        let vars = [
            "JET_CONFIG_PATH",
            "JET_SERVER_HOST",
            "JET_SERVER_PORT",
            "JET_REDIS_URL",
            "JET_RUNTIME_DIR",
            "JET_RUNTIME_MANIFEST_DIR",
            "JET_RUNTIME_CACHE_KEY",
            "HOME",
        ];

        for key in vars {
            unsafe {
                env::remove_var(key);
            }
        }
    }

    #[test]
    #[serial]
    fn runtime_dir_resolution_prefers_env_then_home_then_system() {
        clear_phase_one_env();

        unsafe {
            env::set_var("JET_RUNTIME_DIR", "/tmp/custom-runtime");
        }
        assert_eq!(
            resolve_runtime_install_dir().expect("env runtime dir should work"),
            PathBuf::from("/tmp/custom-runtime")
        );

        unsafe {
            env::remove_var("JET_RUNTIME_DIR");
            env::set_var("HOME", "/home/jet");
        }
        assert_eq!(
            resolve_runtime_install_dir().expect("home fallback should work"),
            PathBuf::from("/home/jet/.jet/runtimes")
        );

        unsafe {
            env::remove_var("HOME");
        }
        assert_eq!(
            resolve_runtime_install_dir().expect("system fallback should work"),
            PathBuf::from("/var/lib/jet/runtimes")
        );
    }

    #[test]
    #[serial]
    fn config_load_uses_defaults_when_no_file_is_present() {
        clear_phase_one_env();
        unsafe {
            env::set_var("HOME", "/home/defaults");
        }

        let config = JetConfig::load().expect("load should succeed with defaults");

        assert_eq!(config.server_host, DEFAULT_SERVER_HOST);
        assert_eq!(config.server_port, DEFAULT_SERVER_PORT);
        assert_eq!(config.redis_url, DEFAULT_REDIS_URL);
        assert_eq!(
            config.runtime_install_dir,
            PathBuf::from("/home/defaults/.jet/runtimes")
        );
        assert_eq!(
            config.runtimes_manifest_dir,
            PathBuf::from("/home/defaults/.jet/runtimes/manifests")
        );
    }

    #[test]
    #[serial]
    fn config_load_reads_toml_and_applies_env_override_priority() {
        clear_phase_one_env();

        let mut file = NamedTempFile::new().expect("temp file should be created");
        writeln!(
            file,
            "server_host = \"0.0.0.0\"\nserver_port = 9000\nredis_url = \"redis://cache:6379\"\nruntime_install_dir = \"/opt/jet/runtimes\"\n"
        )
        .expect("config content should be written");

        unsafe {
            env::set_var("JET_CONFIG_PATH", file.path());
            env::set_var("JET_SERVER_PORT", "4000");
            env::set_var("JET_RUNTIME_CACHE_KEY", "jet:test:version_map");
        }

        let config = JetConfig::load().expect("load should succeed");

        assert_eq!(config.server_host, "0.0.0.0");
        assert_eq!(config.server_port, 4000);
        assert_eq!(config.redis_url, "redis://cache:6379");
        assert_eq!(
            config.runtime_install_dir,
            PathBuf::from("/opt/jet/runtimes")
        );
        assert_eq!(
            config.runtimes_manifest_dir,
            PathBuf::from("/opt/jet/runtimes/manifests")
        );
        assert_eq!(config.runtime_cache_key, "jet:test:version_map");
    }

    #[test]
    #[serial]
    fn config_load_fails_for_invalid_port_override() {
        clear_phase_one_env();

        unsafe {
            env::set_var("JET_SERVER_PORT", "invalid");
        }

        let err = JetConfig::load().expect_err("invalid port must fail");
        assert!(matches!(err, JetError::Config { .. }));
    }

    #[test]
    #[serial]
    fn config_load_fails_for_invalid_toml() {
        clear_phase_one_env();

        let mut file = NamedTempFile::new().expect("temp file should be created");
        writeln!(file, "server_port = not-a-number").expect("invalid content should be written");

        unsafe {
            env::set_var("JET_CONFIG_PATH", file.path());
        }

        let err = JetConfig::load().expect_err("invalid toml should fail");
        assert!(matches!(err, JetError::ConfigParse { .. }));
    }
}
