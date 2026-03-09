use std::fs;
use std::path::Path;

use jet_core::models::FileRequest;

use super::traits::{LanguageBackend, WriteResult};

/// Default backend used for every language that does **not** need
/// special handling (C, C++, Python, Zig, …).
pub struct GenericBackend;

impl LanguageBackend for GenericBackend {
    fn write_files(&self, workspace: &Path, files: &[FileRequest]) -> std::io::Result<WriteResult> {
        let mut primary_file = "main".to_string();

        for (i, file) in files.iter().enumerate() {
            let name = file.name.as_deref().unwrap_or("main");
            let path = workspace.join(name);
            fs::write(&path, &file.content)?;
            if i == 0 {
                primary_file = name.to_string();
            }
        }

        Ok(WriteResult {
            primary_file,
            class_name: None,
        })
    }
}
