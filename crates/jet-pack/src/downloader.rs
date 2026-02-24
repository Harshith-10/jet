use std::{fs, io, io::Read, path::Path};

use indicatif::ProgressBar;

use crate::error::{JetPackError, JetPackResult};

/// Download a URL to a local path, updating a progress bar during download.
///
/// For `file://` URLs this is just a copy with no progress. For HTTP(S) URLs
/// we stream the response body in chunks and increment the progress bar.
pub fn download_to_path_with_progress(
    url: &str,
    destination: &Path,
    progress: &ProgressBar,
) -> JetPackResult<()> {
    if let Some(path) = url.strip_prefix("file://") {
        let metadata = fs::metadata(path).map_err(|source| JetPackError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        progress.set_length(metadata.len());
        fs::copy(path, destination).map_err(|source| JetPackError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        progress.set_position(metadata.len());
        return Ok(());
    }

    let response = reqwest::blocking::get(url).map_err(|source| JetPackError::Http {
        url: url.to_string(),
        source,
    })?;

    let total_size = response.content_length().unwrap_or(0);
    progress.set_length(total_size);

    let mut out = fs::File::create(destination).map_err(|source| JetPackError::Io {
        path: destination.to_path_buf(),
        source,
    })?;

    let mut source = response;
    let mut buf = vec![0u8; 8192];
    loop {
        let n = source.read(&mut buf).map_err(|source| JetPackError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        io::copy(&mut &buf[..n], &mut out).map_err(|source| JetPackError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        progress.inc(n as u64);
    }

    Ok(())
}

/// Simple download without any progress indication (backward compatible).
pub fn download_to_path(url: &str, destination: &Path) -> JetPackResult<()> {
    if let Some(path) = url.strip_prefix("file://") {
        fs::copy(path, destination).map_err(|source| JetPackError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        return Ok(());
    }

    let mut response = reqwest::blocking::get(url).map_err(|source| JetPackError::Http {
        url: url.to_string(),
        source,
    })?;

    let mut out = fs::File::create(destination).map_err(|source| JetPackError::Io {
        path: destination.to_path_buf(),
        source,
    })?;

    io::copy(&mut response, &mut out).map_err(|source| JetPackError::Io {
        path: destination.to_path_buf(),
        source,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn downloads_file_url_into_destination() {
        let dir = tempdir().expect("temp dir should exist");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");

        fs::write(&src, "runtime-data").expect("source file should be written");

        download_to_path(&format!("file://{}", src.display()), &dst).expect("download should pass");

        let actual = fs::read_to_string(dst).expect("destination should be readable");
        assert_eq!(actual, "runtime-data");
    }

    #[test]
    fn downloads_file_url_with_progress() {
        let dir = tempdir().expect("temp dir should exist");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");

        fs::write(&src, "runtime-data-progress").expect("source file should be written");

        let pb = ProgressBar::hidden();
        download_to_path_with_progress(&format!("file://{}", src.display()), &dst, &pb)
            .expect("download with progress should pass");

        let actual = fs::read_to_string(dst).expect("destination should be readable");
        assert_eq!(actual, "runtime-data-progress");
        assert_eq!(pb.position(), 21); // "runtime-data-progress".len()
    }

    #[test]
    fn fails_when_file_source_does_not_exist() {
        let dir = tempdir().expect("temp dir should exist");
        let dst = dir.path().join("dst.txt");

        let result = download_to_path("file:///this/path/does/not/exist", &dst);
        assert!(matches!(result, Err(JetPackError::Io { .. })));
    }
}
