use std::{fs, io, path::Path};

use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;

use crate::error::{JetPackError, JetPackResult};

pub fn extract_archive(archive_path: &Path, destination: &Path) -> JetPackResult<()> {
    fs::create_dir_all(destination).map_err(|source| JetPackError::Io {
        path: destination.to_path_buf(),
        source,
    })?;

    let file_name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    if file_name.ends_with(".tar.gz") {
        return extract_tar_gz(archive_path, destination);
    }

    if file_name.ends_with(".zip") {
        return extract_zip(archive_path, destination);
    }

    Err(JetPackError::UnsupportedArchive {
        path: archive_path.to_path_buf(),
    })
}

fn extract_tar_gz(archive_path: &Path, destination: &Path) -> JetPackResult<()> {
    let file = fs::File::open(archive_path).map_err(|source| JetPackError::Io {
        path: archive_path.to_path_buf(),
        source,
    })?;

    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);

    archive.unpack(destination).map_err(|source| JetPackError::Io {
        path: destination.to_path_buf(),
        source,
    })?;

    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> JetPackResult<()> {
    let file = fs::File::open(archive_path).map_err(|source| JetPackError::Io {
        path: archive_path.to_path_buf(),
        source,
    })?;

    let mut archive = ZipArchive::new(file).map_err(|source| JetPackError::Io {
        path: archive_path.to_path_buf(),
        source: io::Error::other(source.to_string()),
    })?;

    for index in 0..archive.len() {
        let mut zipped = archive.by_index(index).map_err(|source| JetPackError::Io {
            path: archive_path.to_path_buf(),
            source: io::Error::other(source.to_string()),
        })?;

        let Some(enclosed_path) = zipped.enclosed_name() else {
            continue;
        };

        let output_path = destination.join(enclosed_path);

        if zipped.is_dir() {
            fs::create_dir_all(&output_path).map_err(|source| JetPackError::Io {
                path: output_path.clone(),
                source,
            })?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|source| JetPackError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut out = fs::File::create(&output_path).map_err(|source| JetPackError::Io {
            path: output_path.clone(),
            source,
        })?;

        io::copy(&mut zipped, &mut out).map_err(|source| JetPackError::Io {
            path: output_path,
            source,
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::{fs, io::Write};
    use tar::Builder;
    use tempfile::tempdir;

    #[test]
    fn extracts_tar_gz_archive() {
        let dir = tempdir().expect("temp dir should exist");
        let archive = dir.path().join("runtime.tar.gz");
        let extract_to = dir.path().join("out");

        let tar_gz = fs::File::create(&archive).expect("archive should be created");
        let encoder = GzEncoder::new(tar_gz, Compression::default());
        let mut tar = Builder::new(encoder);

        let content = b"hello";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "runtime/bin.txt", &content[..])
            .expect("tar append should work");
        tar.finish().expect("tar should finish");
        let encoder = tar.into_inner().expect("encoder should be returned");
        encoder.finish().expect("gzip should finish");

        extract_archive(&archive, &extract_to).expect("extract should work");

        let extracted = fs::read_to_string(extract_to.join("runtime/bin.txt"))
            .expect("extracted file should exist");
        assert_eq!(extracted, "hello");
    }

    #[test]
    fn fails_for_unsupported_archive_extension() {
        let dir = tempdir().expect("temp dir should exist");
        let archive = dir.path().join("runtime.bin");
        fs::write(&archive, "binary-data").expect("file should be written");

        let result = extract_archive(&archive, &dir.path().join("out"));
        assert!(matches!(result, Err(JetPackError::UnsupportedArchive { .. })));
    }

    #[test]
    fn extracts_zip_archive() {
        let dir = tempdir().expect("temp dir should exist");
        let archive = dir.path().join("runtime.zip");
        let extract_to = dir.path().join("out_zip");

        {
            let file = fs::File::create(&archive).expect("zip should be created");
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("runtime/zip.txt", options)
                .expect("zip file entry should start");
            zip.write_all(b"zip-hello")
                .expect("zip content should be written");
            zip.finish().expect("zip should finish");
        }

        extract_archive(&archive, &extract_to).expect("zip extraction should pass");

        let extracted = fs::read_to_string(extract_to.join("runtime/zip.txt"))
            .expect("zip extracted file should exist");
        assert_eq!(extracted, "zip-hello");
    }
}
