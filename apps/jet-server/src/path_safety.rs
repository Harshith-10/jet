use std::path::{Component, Path, PathBuf};

const MAX_JOB_ID_LEN: usize = 64;

pub fn validate_job_id(job_id: &str) -> Result<(), String> {
    if job_id.is_empty() {
        return Err("job id cannot be empty".to_string());
    }

    if job_id.len() > MAX_JOB_ID_LEN {
        return Err(format!("job id is too long (max: {MAX_JOB_ID_LEN})"));
    }

    if !job_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err("job id contains invalid characters".to_string());
    }

    Ok(())
}

pub fn build_job_workspace_path(jobs_root: &Path, job_id: &str) -> std::io::Result<PathBuf> {
    validate_job_id(job_id).map_err(invalid_input)?;

    std::fs::create_dir_all(jobs_root)?;
    let canonical_jobs_root = jobs_root.canonicalize()?;
    let workspace_dir = canonical_jobs_root.join(job_id);

    let Some(parent) = workspace_dir.parent() else {
        return Err(std::io::Error::other("workspace path missing parent"));
    };

    if parent != canonical_jobs_root {
        return Err(std::io::Error::other(
            "workspace path escaped configured jobs root",
        ));
    }

    Ok(workspace_dir)
}

pub fn sanitize_relative_submission_path(raw_path: &str) -> Result<PathBuf, String> {
    if raw_path.is_empty() {
        return Err("file name cannot be empty".to_string());
    }

    if raw_path.starts_with('/') || raw_path.starts_with('\\') {
        return Err("absolute file paths are not allowed".to_string());
    }

    if raw_path.ends_with('/') || raw_path.ends_with('\\') || raw_path.contains("//") {
        return Err("file path contains empty components".to_string());
    }

    if raw_path.contains('\\') || raw_path.contains(':') || raw_path.contains('\0') {
        return Err("file path contains unsupported characters".to_string());
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(raw_path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {
                return Err("file path cannot contain '.' components".to_string());
            }
            Component::ParentDir => {
                return Err("file path cannot contain '..' components".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute file paths are not allowed".to_string());
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err("file name cannot be empty".to_string());
    }

    Ok(normalized)
}

pub fn prepare_workspace_file_path(
    workspace: &Path,
    submitted_name: Option<&str>,
    default_name: &str,
) -> std::io::Result<(PathBuf, String)> {
    let requested = submitted_name.unwrap_or(default_name);
    let relative_path = sanitize_relative_submission_path(requested).map_err(invalid_input)?;
    let canonical_workspace = workspace.canonicalize()?;
    let full_path = canonical_workspace.join(&relative_path);

    let Some(parent) = full_path.parent() else {
        return Err(std::io::Error::other("workspace file path missing parent"));
    };

    std::fs::create_dir_all(parent)?;
    let canonical_parent = parent.canonicalize()?;
    if !canonical_parent.starts_with(&canonical_workspace) {
        return Err(std::io::Error::other(
            "workspace file path escaped workspace root",
        ));
    }

    Ok((full_path, path_to_string(&relative_path)))
}

fn path_to_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn invalid_input(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::{
        build_job_workspace_path, prepare_workspace_file_path, sanitize_relative_submission_path,
        validate_job_id,
    };

    #[test]
    fn rejects_invalid_job_ids() {
        assert!(validate_job_id("").is_err());
        assert!(validate_job_id("../escape").is_err());
        assert!(validate_job_id("job/child").is_err());
        assert!(validate_job_id("job:child").is_err());
    }

    #[test]
    fn builds_workspace_path_under_jobs_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = build_job_workspace_path(tmp.path(), "job-123").expect("workspace path");

        assert_eq!(path, tmp.path().join("job-123"));
    }

    #[test]
    fn rejects_unsafe_submission_paths() {
        assert!(sanitize_relative_submission_path("../../etc/passwd").is_err());
        assert!(sanitize_relative_submission_path("/etc/passwd").is_err());
        assert!(sanitize_relative_submission_path("nested//file.py").is_err());
        assert!(sanitize_relative_submission_path("dir\\file.py").is_err());
    }

    #[test]
    fn prepares_nested_workspace_file_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (path, relative) = prepare_workspace_file_path(tmp.path(), Some("src/main.py"), "main")
            .expect("workspace path");

        assert_eq!(relative, "src/main.py");
        assert_eq!(path, tmp.path().join("src/main.py"));
        assert!(tmp.path().join("src").exists());
    }
}