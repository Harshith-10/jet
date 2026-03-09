use std::path::Path;

use jet_core::models::{ExecutionLimits, FileRequest};
use jet_pack::RuntimeManifest;

/// Result of writing source files to the workspace.
pub struct WriteResult {
    /// The "primary" file name (e.g. `main.c`, `HelloWorld.java`).
    pub primary_file: String,
    /// An optional "target" name derived from the source (e.g. the Java
    /// public class name `HelloWorld`).  When `None`, the evaluator will
    /// fall back to `primary_file`.
    pub class_name: Option<String>,
}

/// Language-specific hooks consumed by [`super::Evaluator`].
///
/// Every language has a backend that controls how source files are
/// written, how compile/run arguments are assembled, and how resource
/// limits are adjusted before execution.
pub trait LanguageBackend {
    /// Write the submitted source files into `workspace` and return
    /// metadata about the primary file.
    fn write_files(
        &self,
        workspace: &Path,
        files: &[FileRequest],
    ) -> std::io::Result<WriteResult>;

    /// Adjust compilation limits (called before the compile sandbox is
    /// created).  The default implementation is a no-op.
    fn adjust_compile_limits(&self, _limits: &mut ExecutionLimits, _manifest: &RuntimeManifest) {}

    /// Adjust run limits (called before the run sandbox is created).
    /// The default implementation is a no-op.
    fn adjust_run_limits(&self, _limits: &mut ExecutionLimits, _manifest: &RuntimeManifest) {}

    /// Build the full argument list for the **compile** command.
    ///
    /// `template_args` are the args from the manifest after placeholder
    /// substitution.
    fn build_compile_args(
        &self,
        template_args: Vec<String>,
        manifest: &RuntimeManifest,
    ) -> Vec<String> {
        let _ = manifest;
        template_args
    }

    /// Build the full argument list for the **run** command.
    ///
    /// `template_args` are the args from the manifest after placeholder
    /// substitution.
    fn build_run_args(
        &self,
        template_args: Vec<String>,
        manifest: &RuntimeManifest,
    ) -> Vec<String> {
        let _ = manifest;
        template_args
    }
}
