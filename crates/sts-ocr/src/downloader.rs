//! Streaming HTTP model downloader with incremental SHA-256 verification and atomic `.part` swap.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::OcrError;
use crate::manifest::{ModelArtifact, ModelPackSpec, ModelStore};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Progress update emitted while streaming a model pack download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    /// Local filename of the artifact currently being downloaded (`det.onnx`, `rec.onnx`, `dict.txt`).
    pub artifact: String,
    /// Zero-based index of the current artifact within the pack.
    pub artifact_index: usize,
    /// Total number of artifacts in the pack.
    pub artifact_count: usize,
    /// Cumulative bytes downloaded across the pack so far.
    pub bytes_downloaded: u64,
    /// Total expected bytes across the pack.
    pub total_bytes: u64,
}

/// RAII guard that removes any staged `.part` files on early return, error, or cancellation.
struct StagedPartFiles {
    paths: Vec<PathBuf>,
}

impl StagedPartFiles {
    const fn new() -> Self {
        Self { paths: Vec::new() }
    }

    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn disarm(&mut self, path: &Path) {
        self.paths.retain(|entry| entry != path);
    }

    async fn cleanup_all_async(&mut self) {
        let paths = std::mem::take(&mut self.paths);
        for path in paths {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

impl Drop for StagedPartFiles {
    fn drop(&mut self) {
        for path in self.paths.drain(..) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Streams PP-OCRv5 model artifacts to `.part` temporary files, verifies incremental SHA-256
/// checksums, cleans up `.part` files on any failure, and atomically installs on success.
#[derive(Debug, Clone)]
pub struct ModelDownloader {
    client: reqwest::Client,
}

impl ModelDownloader {
    /// Creates a downloader with a default configured [`reqwest::Client`].
    pub fn new() -> Result<Self, OcrError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("square-to-search/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| OcrError::DownloadFailed {
                artifact: "http-client".to_owned(),
                message: err.to_string(),
            })?;
        Ok(Self { client })
    }

    /// Creates a downloader backed by a caller-provided [`reqwest::Client`].
    #[must_use]
    pub const fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Returns a reference to the underlying HTTP client.
    #[must_use]
    pub const fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Downloads and verifies all artifacts in `spec`, installing them atomically into `store`.
    ///
    /// Every artifact is streamed to `<dest>.part` and incrementally hashed with SHA-256.
    /// Final installation via atomic rename happens only after all 3 artifacts pass SHA-256
    /// verification; on any network error, HTTP failure, or checksum mismatch, all `.part` files
    /// are deleted and any pre-existing valid pack files remain completely untouched.
    pub async fn download_pack<F>(
        &self,
        store: &ModelStore,
        spec: &ModelPackSpec,
        on_progress: F,
    ) -> Result<PathBuf, OcrError>
    where
        F: Fn(DownloadProgress) + Send + Sync,
    {
        let dest_dir = store.pack_dir(spec.pack, spec.tier);
        tokio::fs::create_dir_all(&dest_dir)
            .await
            .map_err(|source| OcrError::Io {
                path: dest_dir.clone(),
                source,
            })?;

        let artifacts = spec.artifacts();
        let artifact_count = artifacts.len();
        let mut tracker = PackProgressTracker {
            artifact_count,
            cumulative_bytes: 0,
            total_bytes: spec.total_size_bytes(),
            on_progress: &on_progress,
        };

        let mut part_guard = StagedPartFiles::new();
        let mut staged = Vec::with_capacity(artifact_count);

        for (artifact_index, artifact) in artifacts.iter().enumerate() {
            let final_path = dest_dir.join(&artifact.filename);
            let part_path = dest_dir.join(format!("{}.part", artifact.filename));
            part_guard.track(part_path.clone());

            let download_result = self
                .stream_and_verify_artifact(artifact, &part_path, artifact_index, &mut tracker)
                .await;

            if let Err(err) = download_result {
                part_guard.cleanup_all_async().await;
                return Err(err);
            }

            staged.push((part_path, final_path));
        }

        // All artifacts passed SHA-256 verification; commit them into place atomically.
        for (part_path, final_path) in staged {
            if let Err(source) = tokio::fs::rename(&part_path, &final_path).await {
                part_guard.cleanup_all_async().await;
                return Err(OcrError::Io {
                    path: final_path,
                    source,
                });
            }
            part_guard.disarm(&part_path);
        }

        tracing::info!(
            pack = ?spec.pack,
            tier = ?spec.tier,
            bytes = tracker.cumulative_bytes,
            dir = %dest_dir.display(),
            "installed verified OCR model pack"
        );

        Ok(dest_dir)
    }

    async fn stream_and_verify_artifact<F>(
        &self,
        artifact: &ModelArtifact,
        part_path: &Path,
        artifact_index: usize,
        tracker: &mut PackProgressTracker<'_, F>,
    ) -> Result<(), OcrError>
    where
        F: Fn(DownloadProgress) + Send + Sync,
    {
        tracing::debug!(
            artifact = %artifact.filename,
            url = %artifact.url,
            expected_bytes = artifact.size_bytes,
            "starting OCR artifact download"
        );

        let mut response = self.client.get(&artifact.url).send().await.map_err(|err| {
            OcrError::DownloadFailed {
                artifact: artifact.filename.clone(),
                message: err.to_string(),
            }
        })?;

        if !response.status().is_success() {
            return Err(OcrError::DownloadFailed {
                artifact: artifact.filename.clone(),
                message: format!("HTTP {}", response.status()),
            });
        }

        if artifact.size_bytes == 0
            && let Some(content_len) = response.content_length()
        {
            tracker.add_discovered_total(content_len);
        }

        let mut hasher = Sha256::new();
        let mut artifact_bytes_written: u64 = 0;

        // Scope `file` so the OS handle is guaranteed to close before cleanup or rename (required on Windows).
        {
            let mut file =
                tokio::fs::File::create(part_path)
                    .await
                    .map_err(|source| OcrError::Io {
                        path: part_path.to_path_buf(),
                        source,
                    })?;

            while let Some(chunk) =
                response
                    .chunk()
                    .await
                    .map_err(|err| OcrError::DownloadFailed {
                        artifact: artifact.filename.clone(),
                        message: err.to_string(),
                    })?
            {
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .map_err(|source| OcrError::Io {
                        path: part_path.to_path_buf(),
                        source,
                    })?;

                let chunk_len = chunk.len() as u64;
                artifact_bytes_written = artifact_bytes_written.saturating_add(chunk_len);
                tracker.record_chunk(&artifact.filename, artifact_index, chunk_len);
            }

            file.flush().await.map_err(|source| OcrError::Io {
                path: part_path.to_path_buf(),
                source,
            })?;
            file.sync_all().await.map_err(|source| OcrError::Io {
                path: part_path.to_path_buf(),
                source,
            })?;
        }

        // Emit a progress update even for a 0-byte body so callers always observe each artifact.
        if artifact_bytes_written == 0 {
            tracker.record_chunk(&artifact.filename, artifact_index, 0);
        }

        let actual_sha256 = hex::encode(hasher.finalize());
        if !actual_sha256.eq_ignore_ascii_case(&artifact.sha256) {
            tracing::warn!(
                artifact = %artifact.filename,
                expected = %artifact.sha256,
                actual = %actual_sha256,
                "OCR artifact SHA-256 verification failed"
            );
            return Err(OcrError::ChecksumMismatch {
                artifact: artifact.filename.clone(),
                expected: artifact.sha256.clone(),
                actual: actual_sha256,
            });
        }

        Ok(())
    }
}

struct PackProgressTracker<'a, F> {
    artifact_count: usize,
    cumulative_bytes: u64,
    total_bytes: u64,
    on_progress: &'a F,
}

impl<F: Fn(DownloadProgress) + Send + Sync> PackProgressTracker<'_, F> {
    fn add_discovered_total(&mut self, bytes: u64) {
        self.total_bytes = self.total_bytes.saturating_add(bytes);
    }

    fn record_chunk(&mut self, artifact: &str, artifact_index: usize, chunk_len: u64) {
        self.cumulative_bytes = self.cumulative_bytes.saturating_add(chunk_len);
        if self.cumulative_bytes > self.total_bytes {
            self.total_bytes = self.cumulative_bytes;
        }
        (self.on_progress)(DownloadProgress {
            artifact: artifact.to_owned(),
            artifact_index,
            artifact_count: self.artifact_count,
            bytes_downloaded: self.cumulative_bytes,
            total_bytes: self.total_bytes,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use sha2::{Digest, Sha256};
    use sts_core::types::{OcrLanguagePack, OcrModelTier};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::manifest::{DET_MODEL_FILENAME, DICT_FILENAME, ModelArtifact, REC_MODEL_FILENAME};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn test_spec(
        base_url: &str,
        det_bytes: &[u8],
        rec_bytes: &[u8],
        dict_bytes: &[u8],
    ) -> ModelPackSpec {
        ModelPackSpec {
            pack: OcrLanguagePack::CjkEn,
            tier: OcrModelTier::Mobile,
            version: "test-v5".to_owned(),
            display_name: "Test Pack".to_owned(),
            det: ModelArtifact {
                filename: DET_MODEL_FILENAME.to_owned(),
                url: format!("{base_url}/det.onnx"),
                sha256: sha256_hex(det_bytes),
                size_bytes: det_bytes.len() as u64,
            },
            rec: ModelArtifact {
                filename: REC_MODEL_FILENAME.to_owned(),
                url: format!("{base_url}/rec.onnx"),
                sha256: sha256_hex(rec_bytes),
                size_bytes: rec_bytes.len() as u64,
            },
            dict: ModelArtifact {
                filename: DICT_FILENAME.to_owned(),
                url: format!("{base_url}/dict.txt"),
                sha256: sha256_hex(dict_bytes),
                size_bytes: dict_bytes.len() as u64,
            },
        }
    }

    fn assert_no_part_files(dir: &Path) -> TestResult {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name();
            assert!(
                !name.to_string_lossy().ends_with(".part"),
                "found leftover .part file: {name:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn downloader_verifies_sha256_reports_progress_and_atomically_installs() -> TestResult {
        let server = MockServer::start().await;
        let det_bytes = vec![0x11_u8; 4096];
        let rec_bytes = vec![0x22_u8; 8192];
        let dict_bytes = b"a\nb\nc\n\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e\n".to_vec();

        Mock::given(method("GET"))
            .and(path("/det.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(det_bytes.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rec.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(rec_bytes.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dict.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(dict_bytes.clone()))
            .mount(&server)
            .await;

        let tmp = TempDir::new()?;
        let store = ModelStore::new(tmp.path().to_path_buf());
        let spec = test_spec(&server.uri(), &det_bytes, &rec_bytes, &dict_bytes);
        let downloader = ModelDownloader::new()?;

        let events = Arc::new(Mutex::new(Vec::<DownloadProgress>::new()));
        let events_cb = Arc::clone(&events);
        let installed_dir = downloader
            .download_pack(&store, &spec, move |progress| {
                if let Ok(mut guard) = events_cb.lock() {
                    guard.push(progress);
                }
            })
            .await?;

        assert_eq!(installed_dir, store.pack_dir(spec.pack, spec.tier));
        assert!(store.is_installed(spec.pack, spec.tier));

        let paths = store.pack_paths(spec.pack, spec.tier);
        assert_eq!(fs::read(&paths.det)?, det_bytes);
        assert_eq!(fs::read(&paths.rec)?, rec_bytes);
        assert_eq!(fs::read(&paths.dict)?, dict_bytes);
        assert_no_part_files(&installed_dir)?;

        let recorded = events
            .lock()
            .map_err(|_| "progress mutex poisoned")?
            .clone();
        assert!(!recorded.is_empty(), "expected progress callbacks");

        let expected_total = (det_bytes.len() + rec_bytes.len() + dict_bytes.len()) as u64;
        for pair in recorded.windows(2) {
            assert!(
                pair[1].bytes_downloaded >= pair[0].bytes_downloaded,
                "progress bytes must be monotonic"
            );
        }
        let Some(last) = recorded.last() else {
            return Err("expected at least one progress event".into());
        };
        assert_eq!(last.bytes_downloaded, expected_total);
        assert_eq!(last.total_bytes, expected_total);

        Ok(())
    }

    #[tokio::test]
    async fn downloader_rejects_corrupt_checksum_removes_part_and_preserves_existing_file()
    -> TestResult {
        let server = MockServer::start().await;
        let expected_det = b"EXPECTED_DET_MODEL_V2".to_vec();
        let expected_rec = b"EXPECTED_REC_MODEL_V2".to_vec();
        let expected_dict = b"EXPECTED_DICT_V2".to_vec();

        // Server returns corrupted bytes for det.onnx.
        Mock::given(method("GET"))
            .and(path("/det.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"CORRUPTED_BYTES".to_vec()))
            .mount(&server)
            .await;

        let tmp = TempDir::new()?;
        let store = ModelStore::new(tmp.path().to_path_buf());
        let spec = test_spec(&server.uri(), &expected_det, &expected_rec, &expected_dict);

        // Pre-populate an existing valid pack on disk.
        let pack_dir = store.pack_dir(spec.pack, spec.tier);
        fs::create_dir_all(&pack_dir)?;
        let paths = store.pack_paths(spec.pack, spec.tier);
        fs::write(&paths.det, b"EXISTING_VALID_DET")?;
        fs::write(&paths.rec, b"EXISTING_VALID_REC")?;
        fs::write(&paths.dict, b"EXISTING_VALID_DICT")?;

        let downloader = ModelDownloader::new()?;
        let result = downloader.download_pack(&store, &spec, |_| {}).await;

        assert!(
            matches!(
                result,
                Err(OcrError::ChecksumMismatch { ref artifact, .. }) if artifact == DET_MODEL_FILENAME
            ),
            "expected ChecksumMismatch for det.onnx, got {result:?}"
        );
        assert_no_part_files(&pack_dir)?;
        assert_eq!(fs::read(&paths.det)?, b"EXISTING_VALID_DET");
        assert_eq!(fs::read(&paths.rec)?, b"EXISTING_VALID_REC");
        assert_eq!(fs::read(&paths.dict)?, b"EXISTING_VALID_DICT");
        Ok(())
    }

    #[tokio::test]
    async fn downloader_rejects_corrupt_second_artifact_without_overwriting_existing_pack()
    -> TestResult {
        let server = MockServer::start().await;
        let new_det = b"NEW_VALID_DET_BYTES".to_vec();
        let expected_rec = b"NEW_EXPECTED_REC_BYTES".to_vec();
        let new_dict = b"NEW_VALID_DICT_BYTES".to_vec();

        // First artifact succeeds with valid hash, second artifact is corrupted!
        Mock::given(method("GET"))
            .and(path("/det.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(new_det.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rec.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"TAMPERED_REC".to_vec()))
            .mount(&server)
            .await;

        let tmp = TempDir::new()?;
        let store = ModelStore::new(tmp.path().to_path_buf());
        let spec = test_spec(&server.uri(), &new_det, &expected_rec, &new_dict);

        let pack_dir = store.pack_dir(spec.pack, spec.tier);
        fs::create_dir_all(&pack_dir)?;
        let paths = store.pack_paths(spec.pack, spec.tier);
        fs::write(&paths.det, b"OLD_VALID_DET")?;
        fs::write(&paths.rec, b"OLD_VALID_REC")?;
        fs::write(&paths.dict, b"OLD_VALID_DICT")?;

        let downloader = ModelDownloader::new()?;
        let result = downloader.download_pack(&store, &spec, |_| {}).await;

        assert!(
            matches!(
                result,
                Err(OcrError::ChecksumMismatch { ref artifact, .. }) if artifact == REC_MODEL_FILENAME
            ),
            "expected ChecksumMismatch for rec.onnx, got {result:?}"
        );
        assert_no_part_files(&pack_dir)?;
        // Pack-level atomicity: even det.onnx must still contain OLD_VALID_DET!
        assert_eq!(fs::read(&paths.det)?, b"OLD_VALID_DET");
        assert_eq!(fs::read(&paths.rec)?, b"OLD_VALID_REC");
        assert_eq!(fs::read(&paths.dict)?, b"OLD_VALID_DICT");
        Ok(())
    }

    #[tokio::test]
    async fn downloader_cleans_up_part_on_http_error_and_preserves_existing_pack() -> TestResult {
        let server = MockServer::start().await;
        let new_det = b"NEW_VALID_DET_BYTES".to_vec();
        let new_rec = b"NEW_VALID_REC_BYTES".to_vec();
        let new_dict = b"NEW_VALID_DICT_BYTES".to_vec();

        Mock::given(method("GET"))
            .and(path("/det.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(new_det.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rec.onnx"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let tmp = TempDir::new()?;
        let store = ModelStore::new(tmp.path().to_path_buf());
        let spec = test_spec(&server.uri(), &new_det, &new_rec, &new_dict);

        let pack_dir = store.pack_dir(spec.pack, spec.tier);
        fs::create_dir_all(&pack_dir)?;
        let paths = store.pack_paths(spec.pack, spec.tier);
        fs::write(&paths.det, b"EXISTING_DET")?;
        fs::write(&paths.rec, b"EXISTING_REC")?;
        fs::write(&paths.dict, b"EXISTING_DICT")?;

        let downloader = ModelDownloader::new()?;
        let result = downloader.download_pack(&store, &spec, |_| {}).await;

        assert!(
            matches!(
                result,
                Err(OcrError::DownloadFailed { ref artifact, .. }) if artifact == REC_MODEL_FILENAME
            ),
            "expected DownloadFailed for rec.onnx, got {result:?}"
        );
        assert_no_part_files(&pack_dir)?;
        assert_eq!(fs::read(&paths.det)?, b"EXISTING_DET");
        assert_eq!(fs::read(&paths.rec)?, b"EXISTING_REC");
        assert_eq!(fs::read(&paths.dict)?, b"EXISTING_DICT");
        Ok(())
    }

    #[tokio::test]
    async fn downloader_atomically_replaces_existing_pack_when_all_checksums_match() -> TestResult {
        let server = MockServer::start().await;
        let new_det = b"V2_DET_BYTES".to_vec();
        let new_rec = b"V2_REC_BYTES".to_vec();
        let new_dict = b"V2_DICT_BYTES".to_vec();

        Mock::given(method("GET"))
            .and(path("/det.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(new_det.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rec.onnx"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(new_rec.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dict.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(new_dict.clone()))
            .mount(&server)
            .await;

        let tmp = TempDir::new()?;
        let store = ModelStore::new(tmp.path().to_path_buf());
        let spec = test_spec(&server.uri(), &new_det, &new_rec, &new_dict);

        let pack_dir = store.pack_dir(spec.pack, spec.tier);
        fs::create_dir_all(&pack_dir)?;
        let paths = store.pack_paths(spec.pack, spec.tier);
        fs::write(&paths.det, b"V1_DET")?;
        fs::write(&paths.rec, b"V1_REC")?;
        fs::write(&paths.dict, b"V1_DICT")?;

        let downloader = ModelDownloader::new()?;
        downloader.download_pack(&store, &spec, |_| {}).await?;

        assert_no_part_files(&pack_dir)?;
        assert_eq!(fs::read(&paths.det)?, new_det);
        assert_eq!(fs::read(&paths.rec)?, new_rec);
        assert_eq!(fs::read(&paths.dict)?, new_dict);
        Ok(())
    }
}
