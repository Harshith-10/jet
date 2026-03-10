use std::path::Path;

use crate::{error::JetPackResult, manager::prepare_zig_cache_dir};

use super::{InstallContext, RuntimeInstaller};

/// Installer for Zig-based languages (C, C++, Zig).
///
/// **Post-install**: creates the `zig-cache` directory next to the runtime
/// root. The actual cache population happens server-side inside a sandbox
/// so that Zig's path-dependent cache keys match the sandbox mount layout.
#[derive(Debug, Clone)]
pub struct ZigInstaller;

impl RuntimeInstaller for ZigInstaller {
    fn languages(&self) -> &[&str] {
        &["c", "cpp", "zig"]
    }

    fn post_install(&self, _ctx: &InstallContext, installed_path: &Path) -> JetPackResult<()> {
        prepare_zig_cache_dir(installed_path)?;
        Ok(())
    }
}
