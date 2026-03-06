use std::{
    fs,
    io::{self, Seek, SeekFrom, Write},
    path::Path,
};

use futures::future::join_all;
use indicatif::ProgressBar;

use crate::error::{JetPackError, JetPackResult};

/// Number of parallel connections for chunked downloads.
const NUM_CONNECTIONS: usize = 8;

/// Minimum file size (1 MB) to bother with parallel downloads.
const MIN_PARALLEL_SIZE: u64 = 1_048_576;

// ── public API (blocking) ──────────────────────────────────────────────

/// Download a URL to a local path, updating a progress bar during download.
///
/// For `file://` URLs this is just a copy with no progress. For HTTP(S) URLs
/// we use parallel chunked Range requests when the server supports them,
/// falling back to a single-stream download otherwise.
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

    let rt = build_runtime()?;
    rt.block_on(async_download(url, destination, Some(progress)))
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

    let rt = build_runtime()?;
    rt.block_on(async_download(url, destination, None))
}

// ── internal async implementation ──────────────────────────────────────

fn build_runtime() -> JetPackResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| JetPackError::Io {
            path: std::path::PathBuf::from("<tokio-runtime>"),
            source: e,
        })
}

/// Core async download – attempts parallel chunked fetching first, falls back
/// to a single-stream download when the server doesn't advertise
/// `Accept-Ranges: bytes` or the file is too small.
async fn async_download(
    url: &str,
    destination: &Path,
    progress: Option<&ProgressBar>,
) -> JetPackResult<()> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;

    // Probe the server for size & Range support.
    let head = client
        .head(url)
        .send()
        .await
        .map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;

    let total_size = head.content_length().unwrap_or(0);
    let accepts_ranges = head
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "bytes");

    if let Some(pb) = progress {
        pb.set_length(total_size);
    }

    if accepts_ranges && total_size >= MIN_PARALLEL_SIZE {
        parallel_download(&client, url, destination, total_size, progress).await
    } else {
        single_stream_download(&client, url, destination, progress).await
    }
}

/// Download the file in `NUM_CONNECTIONS` parallel Range-based chunks, then
/// stitch them together on disk.
async fn parallel_download(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    total_size: u64,
    progress: Option<&ProgressBar>,
) -> JetPackResult<()> {
    use futures::StreamExt;

    let chunk_size = total_size / NUM_CONNECTIONS as u64;
    let dest = destination.to_path_buf();

    // Pre-allocate the output file.
    let file = fs::File::create(&dest).map_err(|source| JetPackError::Io {
        path: dest.clone(),
        source,
    })?;
    file.set_len(total_size)
        .map_err(|source| JetPackError::Io {
            path: dest.clone(),
            source,
        })?;
    drop(file);

    // Spawn one task per chunk.
    let mut handles = Vec::with_capacity(NUM_CONNECTIONS);
    for i in 0..NUM_CONNECTIONS {
        let start = i as u64 * chunk_size;
        let end = if i == NUM_CONNECTIONS - 1 {
            total_size - 1
        } else {
            start + chunk_size - 1
        };

        let client = client.clone();
        let url = url.to_string();
        let dest = dest.clone();
        let progress = progress.cloned();

        handles.push(tokio::spawn(async move {
            let resp = client
                .get(&url)
                .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
                .send()
                .await
                .map_err(|source| JetPackError::Http {
                    url: url.clone(),
                    source,
                })?;

            let mut stream = resp.bytes_stream();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .map_err(|source| JetPackError::Io {
                    path: dest.clone(),
                    source,
                })?;
            file.seek(SeekFrom::Start(start))
                .map_err(|source| JetPackError::Io {
                    path: dest.clone(),
                    source,
                })?;

            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|source| JetPackError::Http {
                    url: url.clone(),
                    source,
                })?;
                file.write_all(&bytes).map_err(|source| JetPackError::Io {
                    path: dest.clone(),
                    source,
                })?;
                if let Some(ref pb) = progress {
                    pb.inc(bytes.len() as u64);
                }
            }

            Ok::<(), JetPackError>(())
        }));
    }

    // Collect results; propagate the first error.
    let results = join_all(handles).await;
    for result in results {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(JetPackError::Io {
                    path: dest,
                    source: io::Error::new(io::ErrorKind::Other, e.to_string()),
                });
            }
        }
    }

    Ok(())
}

/// Fallback: single-stream download (e.g. when Range is unsupported).
async fn single_stream_download(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    progress: Option<&ProgressBar>,
) -> JetPackResult<()> {
    use futures::StreamExt;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;

    if progress.is_some() && resp.content_length().is_some() {
        if let Some(pb) = progress {
            pb.set_length(resp.content_length().unwrap());
        }
    }

    let mut file = fs::File::create(destination).map_err(|source| JetPackError::Io {
        path: destination.to_path_buf(),
        source,
    })?;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|source| JetPackError::Http {
            url: url.to_string(),
            source,
        })?;
        file.write_all(&bytes).map_err(|source| JetPackError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        if let Some(pb) = progress {
            pb.inc(bytes.len() as u64);
        }
    }

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
