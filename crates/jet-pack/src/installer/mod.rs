mod java;
mod rust;
mod zig;

pub use java::JavaInstaller;
pub use rust::RustInstaller;
pub use zig::ZigInstaller;

use std::path::Path;

use crate::{error::JetPackResult, manifest::RuntimeManifest};

/// Context passed to installer hooks during the install lifecycle.
pub struct InstallContext<'a> {
    /// Root directory where all runtimes are installed.
    pub runtime_dir: &'a Path,
    /// The manifest being installed.
    pub manifest: &'a RuntimeManifest,
    /// Target CPU architecture (e.g., `"x86_64"`, `"aarch64"`).
    pub arch: &'a str,
}

/// Trait for language-specific install hooks.
///
/// Mirrors the [`RuntimeUpdater`](crate::updater::RuntimeUpdater) pattern:
/// each implementation handles one or more languages and provides
/// pre/post-install hooks that [`PackageManager`](crate::PackageManager)
/// calls automatically during the install lifecycle.
///
/// # Adding a new installer
///
/// 1. Create a new file under `installer/` (e.g. `installer/ruby.rs`).
/// 2. Implement `RuntimeInstaller` for your struct.
/// 3. Re-export it from this module.
/// 4. Register it in [`get_installer_for`] below.
pub trait RuntimeInstaller {
    /// The canonical language names this installer handles.
    fn languages(&self) -> &[&str];

    /// Called **before** the archive is downloaded and extracted.
    ///
    /// Use this for pre-install cleanup — for example, removing old Java
    /// major versions so only the latest patch is kept.
    fn pre_install(&self, ctx: &InstallContext) -> JetPackResult<()> {
        let _ = ctx;
        Ok(())
    }

    /// Called **after** the archive has been successfully extracted.
    ///
    /// Use this for post-install setup — for example, creating the
    /// `zig-cache` directory next to the runtime root.
    fn post_install(&self, ctx: &InstallContext, installed_path: &Path) -> JetPackResult<()> {
        let _ = (ctx, installed_path);
        Ok(())
    }
}

/// Default no-op installer for languages without special requirements.
pub struct DefaultInstaller;

impl RuntimeInstaller for DefaultInstaller {
    fn languages(&self) -> &[&str] {
        &[]
    }
}

/// Returns the appropriate [`RuntimeInstaller`] for a given canonical
/// language name.
///
/// Unknown languages fall back to [`DefaultInstaller`] (no-op hooks).
pub fn get_installer_for(language: &str) -> Box<dyn RuntimeInstaller> {
    match language {
        "java" => Box::new(JavaInstaller::default()),
        "rust" => Box::new(RustInstaller),
        "c" | "cpp" | "zig" => Box::new(ZigInstaller),
        _ => Box::new(DefaultInstaller),
    }
}
