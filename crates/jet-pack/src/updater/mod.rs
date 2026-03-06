mod java_corretto;
mod python_standalone;
mod zig;

pub use java_corretto::{JavaCorrettoUpdater, parse_corretto_release_manifest};
pub use python_standalone::{PythonStandaloneUpdater, parse_python_release_manifests};
pub use zig::{ZigUpdater, parse_zig_index};

use serde_json::Value;

use crate::{
    error::{JetPackError, JetPackResult},
    manifest::RuntimeManifest,
};

#[derive(Debug, Clone)]
pub struct UpdatedManifest {
    pub file_name: String,
    pub manifest: RuntimeManifest,
}

pub trait RuntimeUpdater {
    fn language(&self) -> &'static str;
    fn fetch_updated_manifests(&self) -> JetPackResult<Vec<UpdatedManifest>>;
}

/// Selects which language updaters to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateTarget {
    Java,
    Python,
    Zig,
    All,
}

/// Returns the list of updaters for the given target.
pub fn get_updaters(target: UpdateTarget) -> Vec<Box<dyn RuntimeUpdater>> {
    match target {
        UpdateTarget::Java => vec![Box::new(JavaCorrettoUpdater::default())],
        UpdateTarget::Python => vec![Box::new(PythonStandaloneUpdater)],
        UpdateTarget::Zig => vec![Box::new(ZigUpdater)],
        UpdateTarget::All => vec![
            Box::new(JavaCorrettoUpdater::default()),
            Box::new(PythonStandaloneUpdater),
            Box::new(ZigUpdater),
        ],
    }
}

pub(crate) fn fetch_json(url: &str) -> JetPackResult<Value> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("jet-pack-updater")
        .build()
        .map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;

    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;

    let text = response.text().map_err(|source| JetPackError::Http {
        url: url.to_string(),
        source,
    })?;

    serde_json::from_str(&text).map_err(|error| JetPackError::Serialization {
        message: format!("failed to parse JSON response from {url}: {error}"),
    })
}
