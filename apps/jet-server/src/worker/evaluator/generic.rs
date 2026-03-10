use std::fs;
use std::path::Path;

use jet_core::models::FileRequest;

use crate::path_safety::prepare_workspace_file_path;

use super::traits::{LanguageBackend, WriteResult};

/// Default backend used for every language that does **not** need
/// special handling (C, C++, Python, Zig, …).
pub struct GenericBackend;

impl LanguageBackend for GenericBackend {
    fn write_files(&self, workspace: &Path, files: &[FileRequest]) -> std::io::Result<WriteResult> {
        let mut primary_file = "main".to_string();

        for (i, file) in files.iter().enumerate() {
            let (path, relative_name) =
                prepare_workspace_file_path(workspace, file.name.as_deref(), "main")?;
            fs::write(&path, &file.content)?;
            if i == 0 {
                primary_file = relative_name;
            }
        }

        Ok(WriteResult {
            primary_file,
            class_name: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_directory_traversal() {
        let backend = GenericBackend;
        let tmp = tempfile::tempdir().unwrap();
        let files = vec![FileRequest {
            name: Some("../../etc/passwd".to_string()),
            content: "owned".to_string(),
            encoding: None,
        }];

        let err = backend.write_files(tmp.path(), &files).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_absolute_paths() {
        let backend = GenericBackend;
        let tmp = tempfile::tempdir().unwrap();
        let files = vec![FileRequest {
            name: Some("/tmp/pwned.py".to_string()),
            content: "owned".to_string(),
            encoding: None,
        }];

        let err = backend.write_files(tmp.path(), &files).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
