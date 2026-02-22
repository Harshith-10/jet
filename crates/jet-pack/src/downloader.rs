use std::{fs, io, path::Path};

use crate::error::{JetPackError, JetPackResult};

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
    fn fails_when_file_source_does_not_exist() {
        let dir = tempdir().expect("temp dir should exist");
        let dst = dir.path().join("dst.txt");

        let result = download_to_path("file:///this/path/does/not/exist", &dst);
        assert!(matches!(result, Err(JetPackError::Io { .. })));
    }
}
