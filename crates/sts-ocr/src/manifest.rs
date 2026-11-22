//! Pinned PP-OCRv5 ONNX model manifest and local [`ModelStore`] with atomic offline import.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use sts_core::types::{OcrLanguagePack, OcrModelTier};

use crate::OcrError;

/// Local filename for the text detection ONNX model inside each pack directory.
pub const DET_MODEL_FILENAME: &str = "det.onnx";

/// Local filename for the text recognition ONNX model inside each pack directory.
pub const REC_MODEL_FILENAME: &str = "rec.onnx";

/// Local filename for the character dictionary inside each pack directory.
pub const DICT_FILENAME: &str = "dict.txt";

/// Required filenames that make a pack directory valid and installed.
pub const REQUIRED_PACK_FILES: [&str; 3] = [DET_MODEL_FILENAME, REC_MODEL_FILENAME, DICT_FILENAME];

const MODELSCOPE_BASE_URL: &str =
    "https://www.modelscope.cn/api/v1/models/greatv/oar-ocr/repo?Revision=master&FilePath=";

const PP_OCRV5_VERSION: &str = "pp-ocrv5-oar-0.11.0";

// Upstream PP-OCRv5 ONNX files pinned from `oar-ocr 0.11.0` official ModelScope registry.
const MOBILE_DET_REMOTE: &str = "pp-ocrv5_mobile_det.onnx";
const MOBILE_DET_SHA256: &str = "1eb7b4f7ab657ebd1c66d5f79bca7497f29768a2e3c15e52daecbba1a8e4a039";
const MOBILE_DET_SIZE: u64 = 4_826_518;

const SERVER_DET_REMOTE: &str = "pp-ocrv5_server_det.onnx";
const SERVER_DET_SHA256: &str = "9a910baffbefb807ff2f7bfaa72910e3e470bd17014d798386d87bb46f442839";
const SERVER_DET_SIZE: u64 = 88_116_836;

const CJK_MOBILE_REC_REMOTE: &str = "pp-ocrv5_mobile_rec.onnx";
const CJK_MOBILE_REC_SHA256: &str =
    "243a0f06d826761323e9045e9b113ab2c191c3aa50565585e628300b8eda0224";
const CJK_MOBILE_REC_SIZE: u64 = 16_562_373;

const CJK_SERVER_REC_REMOTE: &str = "pp-ocrv5_server_rec.onnx";
const CJK_SERVER_REC_SHA256: &str =
    "4bfffad2c62eb1340250455856978fb9fb19cb4776b264ae3c2f91c35fbb40b4";
const CJK_SERVER_REC_SIZE: u64 = 84_502_992;

const CJK_DICT_REMOTE: &str = "ppocrv5_dict.txt";
const CJK_DICT_SHA256: &str = "d1979e9f794c464c0d2e0b70a7fe14dd978e9dc644c0e71f14158cdf8342af1b";
const CJK_DICT_SIZE: u64 = 74_012;

const KOREAN_REC_REMOTE: &str = "korean_pp-ocrv5_mobile_rec.onnx";
const KOREAN_REC_SHA256: &str = "2d7ed96308065a86103325d22af07a88c4d06afc009f21602a4882342c0cc054";
const KOREAN_REC_SIZE: u64 = 13_446_374;

const KOREAN_DICT_REMOTE: &str = "ppocrv5_korean_dict.txt";
const KOREAN_DICT_SHA256: &str = "a88071c68c01707489baa79ebe0405b7beb5cca229f4fc94cc3ef992328802d7";
const KOREAN_DICT_SIZE: u64 = 47_451;

const LATIN_REC_REMOTE: &str = "latin_pp-ocrv5_mobile_rec.onnx";
const LATIN_REC_SHA256: &str = "e3a6bfeea1c8a01d6fccfd480a0bd363fd907f8c65931e228bb2736f5c3e142f";
const LATIN_REC_SIZE: u64 = 8_069_614;

const LATIN_DICT_REMOTE: &str = "ppocrv5_latin_dict.txt";
const LATIN_DICT_SHA256: &str = "ccbcc45730b3fbbd9050c5bc74db6a99067141ef1035e3d14889a84a6b9b1aff";
const LATIN_DICT_SIZE: u64 = 2_616;

fn modelscope_artifact(
    local_filename: &str,
    remote_name: &str,
    sha256: &str,
    size_bytes: u64,
) -> ModelArtifact {
    ModelArtifact {
        filename: local_filename.to_owned(),
        url: format!("{MODELSCOPE_BASE_URL}{remote_name}"),
        sha256: sha256.to_owned(),
        size_bytes,
    }
}

fn pack_slug(pack: OcrLanguagePack) -> &'static str {
    match pack {
        OcrLanguagePack::CjkEn => "cjk_en",
        OcrLanguagePack::Korean => "korean",
        OcrLanguagePack::VietnameseLatin => "vietnamese_latin",
    }
}

fn tier_slug(tier: OcrModelTier) -> &'static str {
    match tier {
        OcrModelTier::Mobile => "mobile",
        OcrModelTier::Server => "server",
    }
}

/// RAII cleanup guard for staged `.part` files so failures never leave partial artifacts behind.
struct PartCleanupGuard {
    paths: Vec<PathBuf>,
}

impl PartCleanupGuard {
    const fn new() -> Self {
        Self { paths: Vec::new() }
    }

    fn push(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn disarm_path(&mut self, path: &Path) {
        self.paths.retain(|p| p != path);
    }
}

impl Drop for PartCleanupGuard {
    fn drop(&mut self) {
        for path in self.paths.drain(..) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Resolved filesystem paths for a single installed model pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackFilePaths {
    pub det: PathBuf,
    pub rec: PathBuf,
    pub dict: PathBuf,
}

/// Single downloadable file belonging to an OCR model pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArtifact {
    /// Local filename stored inside the pack directory (`det.onnx`, `rec.onnx`, or `dict.txt`).
    pub filename: String,
    /// Upstream HTTPS download URL.
    pub url: String,
    /// Expected lowercase hex-encoded SHA-256 digest (64 characters).
    pub sha256: String,
    /// Expected size in bytes.
    pub size_bytes: u64,
}

/// Complete specification of a `(OcrLanguagePack, OcrModelTier)` PP-OCRv5 pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPackSpec {
    pub pack: OcrLanguagePack,
    pub tier: OcrModelTier,
    pub version: String,
    pub display_name: String,
    pub det: ModelArtifact,
    pub rec: ModelArtifact,
    pub dict: ModelArtifact,
}

impl ModelPackSpec {
    /// Returns the 3 artifacts (`det`, `rec`, `dict`) in download order.
    #[must_use]
    pub const fn artifacts(&self) -> [&ModelArtifact; 3] {
        [&self.det, &self.rec, &self.dict]
    }

    /// Returns the total expected download size in bytes across all 3 artifacts.
    #[must_use]
    pub const fn total_size_bytes(&self) -> u64 {
        self.det
            .size_bytes
            .saturating_add(self.rec.size_bytes)
            .saturating_add(self.dict.size_bytes)
    }
}

/// Installation status of a single model pack for Settings UI and runtime checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackInstallStatus {
    pub pack: OcrLanguagePack,
    pub tier: OcrModelTier,
    pub display_name: String,
    pub installed: bool,
    pub expected_size_bytes: u64,
    pub installed_size_bytes: u64,
}

/// Pinned catalog of available PP-OCRv5 ONNX model packs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelManifest {
    packs: Vec<ModelPackSpec>,
}

impl ModelManifest {
    /// Constructs a manifest from an explicit list of pack specifications.
    #[must_use]
    pub const fn new(packs: Vec<ModelPackSpec>) -> Self {
        Self { packs }
    }

    /// Built-in manifest pinning all 6 PP-OCRv5 packs (`CjkEn`, `Korean`, `VietnameseLatin` ×
    /// `Mobile`, `Server`) with upstream `oar-ocr` ModelScope URLs, exact sizes, and SHA-256
    /// checksums.
    #[must_use]
    pub fn builtin() -> Self {
        let mobile_det = || {
            modelscope_artifact(
                DET_MODEL_FILENAME,
                MOBILE_DET_REMOTE,
                MOBILE_DET_SHA256,
                MOBILE_DET_SIZE,
            )
        };
        let server_det = || {
            modelscope_artifact(
                DET_MODEL_FILENAME,
                SERVER_DET_REMOTE,
                SERVER_DET_SHA256,
                SERVER_DET_SIZE,
            )
        };

        // Note: upstream PaddleOCR PP-OCRv5 ships dedicated mobile recognition checkpoints for
        // Korean and Latin/Vietnamese; their `Server` packs pair those recognizers with the
        // higher-accuracy `pp-ocrv5_server_det` detector for stylized comic typography.
        Self::new(vec![
            ModelPackSpec {
                pack: OcrLanguagePack::CjkEn,
                tier: OcrModelTier::Mobile,
                version: PP_OCRV5_VERSION.to_owned(),
                display_name: "CJK + EN (Mobile)".to_owned(),
                det: mobile_det(),
                rec: modelscope_artifact(
                    REC_MODEL_FILENAME,
                    CJK_MOBILE_REC_REMOTE,
                    CJK_MOBILE_REC_SHA256,
                    CJK_MOBILE_REC_SIZE,
                ),
                dict: modelscope_artifact(
                    DICT_FILENAME,
                    CJK_DICT_REMOTE,
                    CJK_DICT_SHA256,
                    CJK_DICT_SIZE,
                ),
            },
            ModelPackSpec {
                pack: OcrLanguagePack::CjkEn,
                tier: OcrModelTier::Server,
                version: PP_OCRV5_VERSION.to_owned(),
                display_name: "CJK + EN (Server)".to_owned(),
                det: server_det(),
                rec: modelscope_artifact(
                    REC_MODEL_FILENAME,
                    CJK_SERVER_REC_REMOTE,
                    CJK_SERVER_REC_SHA256,
                    CJK_SERVER_REC_SIZE,
                ),
                dict: modelscope_artifact(
                    DICT_FILENAME,
                    CJK_DICT_REMOTE,
                    CJK_DICT_SHA256,
                    CJK_DICT_SIZE,
                ),
            },
            ModelPackSpec {
                pack: OcrLanguagePack::Korean,
                tier: OcrModelTier::Mobile,
                version: PP_OCRV5_VERSION.to_owned(),
                display_name: "Korean (Mobile)".to_owned(),
                det: mobile_det(),
                rec: modelscope_artifact(
                    REC_MODEL_FILENAME,
                    KOREAN_REC_REMOTE,
                    KOREAN_REC_SHA256,
                    KOREAN_REC_SIZE,
                ),
                dict: modelscope_artifact(
                    DICT_FILENAME,
                    KOREAN_DICT_REMOTE,
                    KOREAN_DICT_SHA256,
                    KOREAN_DICT_SIZE,
                ),
            },
            ModelPackSpec {
                pack: OcrLanguagePack::Korean,
                tier: OcrModelTier::Server,
                version: PP_OCRV5_VERSION.to_owned(),
                display_name: "Korean (Server)".to_owned(),
                det: server_det(),
                rec: modelscope_artifact(
                    REC_MODEL_FILENAME,
                    KOREAN_REC_REMOTE,
                    KOREAN_REC_SHA256,
                    KOREAN_REC_SIZE,
                ),
                dict: modelscope_artifact(
                    DICT_FILENAME,
                    KOREAN_DICT_REMOTE,
                    KOREAN_DICT_SHA256,
                    KOREAN_DICT_SIZE,
                ),
            },
            ModelPackSpec {
                pack: OcrLanguagePack::VietnameseLatin,
                tier: OcrModelTier::Mobile,
                version: PP_OCRV5_VERSION.to_owned(),
                display_name: "Vietnamese / Latin (Mobile)".to_owned(),
                det: mobile_det(),
                rec: modelscope_artifact(
                    REC_MODEL_FILENAME,
                    LATIN_REC_REMOTE,
                    LATIN_REC_SHA256,
                    LATIN_REC_SIZE,
                ),
                dict: modelscope_artifact(
                    DICT_FILENAME,
                    LATIN_DICT_REMOTE,
                    LATIN_DICT_SHA256,
                    LATIN_DICT_SIZE,
                ),
            },
            ModelPackSpec {
                pack: OcrLanguagePack::VietnameseLatin,
                tier: OcrModelTier::Server,
                version: PP_OCRV5_VERSION.to_owned(),
                display_name: "Vietnamese / Latin (Server)".to_owned(),
                det: server_det(),
                rec: modelscope_artifact(
                    REC_MODEL_FILENAME,
                    LATIN_REC_REMOTE,
                    LATIN_REC_SHA256,
                    LATIN_REC_SIZE,
                ),
                dict: modelscope_artifact(
                    DICT_FILENAME,
                    LATIN_DICT_REMOTE,
                    LATIN_DICT_SHA256,
                    LATIN_DICT_SIZE,
                ),
            },
        ])
    }

    /// Returns all packs declared in this manifest.
    #[must_use]
    pub fn packs(&self) -> &[ModelPackSpec] {
        &self.packs
    }

    /// Looks up a pack specification by language pack and accuracy tier.
    #[must_use]
    pub fn find(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> Option<&ModelPackSpec> {
        self.packs
            .iter()
            .find(|spec| spec.pack == pack && spec.tier == tier)
    }
}

/// Local on-disk directory manager for installed PP-OCRv5 ONNX model packs.
#[derive(Debug, Clone)]
pub struct ModelStore {
    root: PathBuf,
}

impl ModelStore {
    /// Creates a store rooted at an explicit directory path.
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolves the standard per-user OS local data directory for OCR models.
    pub fn default_os_dir() -> Result<PathBuf, OcrError> {
        let dirs =
            ProjectDirs::from("dev", "square-to-search", "square-to-search").ok_or_else(|| {
                OcrError::Io {
                    path: PathBuf::new(),
                    source: io::Error::new(
                        io::ErrorKind::NotFound,
                        "unable to determine OS local data directory for OCR models",
                    ),
                }
            })?;
        Ok(dirs.data_local_dir().join("models"))
    }

    /// Returns the root directory managed by this store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the subdirectory for a specific `(pack, tier)` combination.
    #[must_use]
    pub fn pack_dir(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> PathBuf {
        self.root.join(pack_slug(pack)).join(tier_slug(tier))
    }

    /// Returns the resolved file paths (`det.onnx`, `rec.onnx`, `dict.txt`) inside `pack_dir`.
    #[must_use]
    pub fn pack_paths(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> PackFilePaths {
        let dir = self.pack_dir(pack, tier);
        PackFilePaths {
            det: dir.join(DET_MODEL_FILENAME),
            rec: dir.join(REC_MODEL_FILENAME),
            dict: dir.join(DICT_FILENAME),
        }
    }

    /// Returns `true` when all 3 required pack files exist as non-empty regular files.
    #[must_use]
    pub fn is_installed(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> bool {
        let dir = self.pack_dir(pack, tier);
        REQUIRED_PACK_FILES.iter().all(|name| {
            fs::metadata(dir.join(name)).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        })
    }

    /// Computes installation statuses for every pack listed in `manifest`.
    #[must_use]
    pub fn list_statuses(&self, manifest: &ModelManifest) -> Vec<PackInstallStatus> {
        manifest
            .packs()
            .iter()
            .map(|spec| {
                let installed = self.is_installed(spec.pack, spec.tier);
                let installed_size_bytes = if installed {
                    let dir = self.pack_dir(spec.pack, spec.tier);
                    REQUIRED_PACK_FILES
                        .iter()
                        .filter_map(|name| fs::metadata(dir.join(name)).ok())
                        .map(|meta| meta.len())
                        .fold(0_u64, u64::saturating_add)
                } else {
                    0
                };
                PackInstallStatus {
                    pack: spec.pack,
                    tier: spec.tier,
                    display_name: spec.display_name.clone(),
                    installed,
                    expected_size_bytes: spec.total_size_bytes(),
                    installed_size_bytes,
                }
            })
            .collect()
    }

    /// Removes an installed pack directory if present (idempotent when absent).
    pub fn delete_pack(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> Result<(), OcrError> {
        let dir = self.pack_dir(pack, tier);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(OcrError::Io { path: dir, source }),
        }
    }

    /// Validates that `src_dir` contains non-empty `det.onnx`, `rec.onnx`, and `dict.txt` files
    /// and atomically imports them into `pack_dir(pack, tier)`.
    pub fn import_offline_dir(
        &self,
        pack: OcrLanguagePack,
        tier: OcrModelTier,
        src_dir: &Path,
    ) -> Result<(), OcrError> {
        let src_meta = fs::metadata(src_dir).map_err(|err| {
            OcrError::InvalidOfflineDir(format!("cannot access `{}`: {err}", src_dir.display()))
        })?;
        if !src_meta.is_dir() {
            return Err(OcrError::InvalidOfflineDir(format!(
                "`{}` is not a directory",
                src_dir.display()
            )));
        }

        // Validate every required source file before modifying the destination directory.
        for filename in REQUIRED_PACK_FILES {
            let src_file = src_dir.join(filename);
            let meta = fs::metadata(&src_file).map_err(|_| {
                OcrError::InvalidOfflineDir(format!(
                    "missing required file `{filename}` in `{}`",
                    src_dir.display()
                ))
            })?;
            if !meta.is_file() {
                return Err(OcrError::InvalidOfflineDir(format!(
                    "`{}` is not a regular file",
                    src_file.display()
                )));
            }
            if meta.len() == 0 {
                return Err(OcrError::InvalidOfflineDir(format!(
                    "required file `{filename}` in `{}` is empty",
                    src_dir.display()
                )));
            }
        }

        let dest_dir = self.pack_dir(pack, tier);
        fs::create_dir_all(&dest_dir).map_err(|source| OcrError::Io {
            path: dest_dir.clone(),
            source,
        })?;

        let mut cleanup = PartCleanupGuard::new();
        let mut staged = Vec::with_capacity(REQUIRED_PACK_FILES.len());

        // Stage all files to `.part` first so existing valid files are untouched on any error.
        for filename in REQUIRED_PACK_FILES {
            let src_path = src_dir.join(filename);
            let final_path = dest_dir.join(filename);
            let part_path = dest_dir.join(format!("{filename}.part"));
            cleanup.push(part_path.clone());

            copy_and_sync_file(&src_path, &part_path)?;
            staged.push((part_path, final_path));
        }

        for (part_path, final_path) in staged {
            fs::rename(&part_path, &final_path).map_err(|source| OcrError::Io {
                path: final_path.clone(),
                source,
            })?;
            cleanup.disarm_path(&part_path);
        }

        Ok(())
    }
}

fn copy_and_sync_file(src: &Path, dst: &Path) -> Result<(), OcrError> {
    let mut reader = File::open(src).map_err(|source| OcrError::Io {
        path: src.to_path_buf(),
        source,
    })?;
    let mut writer = File::create(dst).map_err(|source| OcrError::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    io::copy(&mut reader, &mut writer).map_err(|source| OcrError::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    writer.sync_all().map_err(|source| OcrError::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn builtin_manifest_contains_all_six_packs_with_valid_sha256_and_urls() {
        let manifest = ModelManifest::builtin();
        assert_eq!(manifest.packs().len(), 6);

        let all_packs = [
            OcrLanguagePack::CjkEn,
            OcrLanguagePack::Korean,
            OcrLanguagePack::VietnameseLatin,
        ];
        let all_tiers = [OcrModelTier::Mobile, OcrModelTier::Server];

        for pack in all_packs {
            for tier in all_tiers {
                let spec = manifest.find(pack, tier);
                assert!(
                    spec.is_some(),
                    "missing manifest entry for {pack:?}/{tier:?}"
                );
                if let Some(spec) = spec {
                    assert_eq!(spec.det.filename, DET_MODEL_FILENAME);
                    assert_eq!(spec.rec.filename, REC_MODEL_FILENAME);
                    assert_eq!(spec.dict.filename, DICT_FILENAME);
                    assert!(spec.total_size_bytes() > 0);
                    for artifact in spec.artifacts() {
                        assert!(
                            artifact.url.starts_with("https://"),
                            "artifact URL must use HTTPS: {}",
                            artifact.url
                        );
                        assert_eq!(
                            artifact.sha256.len(),
                            64,
                            "artifact SHA-256 must be 64 hex chars: {}",
                            artifact.sha256
                        );
                        assert!(
                            artifact
                                .sha256
                                .chars()
                                .all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                            "artifact SHA-256 must be lowercase hex: {}",
                            artifact.sha256
                        );
                        assert!(artifact.size_bytes > 0);
                    }
                }
            }
        }
    }

    #[test]
    fn model_store_pack_dir_separates_packs_and_lists_uninstalled_statuses() -> TestResult {
        let tmp = TempDir::new()?;
        let store = ModelStore::new(tmp.path().to_path_buf());
        let manifest = ModelManifest::builtin();

        let cjk_mobile = store.pack_dir(OcrLanguagePack::CjkEn, OcrModelTier::Mobile);
        let cjk_server = store.pack_dir(OcrLanguagePack::CjkEn, OcrModelTier::Server);
        let ko_mobile = store.pack_dir(OcrLanguagePack::Korean, OcrModelTier::Mobile);
        assert_ne!(cjk_mobile, cjk_server);
        assert_ne!(cjk_mobile, ko_mobile);

        assert!(!store.is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile));

        let statuses = store.list_statuses(&manifest);
        assert_eq!(statuses.len(), 6);
        assert!(
            statuses
                .iter()
                .all(|s| !s.installed && s.installed_size_bytes == 0)
        );
        assert!(statuses.iter().all(|s| s.expected_size_bytes > 0));
        Ok(())
    }

    #[test]
    fn offline_import_validates_required_files_and_copies_into_store() -> TestResult {
        let store_dir = TempDir::new()?;
        let src_dir = TempDir::new()?;
        let store = ModelStore::new(store_dir.path().to_path_buf());

        // Non-existent directory fails cleanly.
        let missing = src_dir.path().join("does-not-exist");
        assert!(matches!(
            store.import_offline_dir(OcrLanguagePack::CjkEn, OcrModelTier::Mobile, &missing),
            Err(OcrError::InvalidOfflineDir(_))
        ));

        // Missing dict.txt fails cleanly and leaves pack uninstalled.
        fs::write(src_dir.path().join(DET_MODEL_FILENAME), b"fake-det-bytes")?;
        fs::write(src_dir.path().join(REC_MODEL_FILENAME), b"fake-rec-bytes")?;
        assert!(matches!(
            store.import_offline_dir(OcrLanguagePack::CjkEn, OcrModelTier::Mobile, src_dir.path()),
            Err(OcrError::InvalidOfflineDir(_))
        ));
        assert!(!store.is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile));

        // Empty dict.txt fails cleanly.
        fs::write(src_dir.path().join(DICT_FILENAME), b"")?;
        assert!(matches!(
            store.import_offline_dir(OcrLanguagePack::CjkEn, OcrModelTier::Mobile, src_dir.path()),
            Err(OcrError::InvalidOfflineDir(_))
        ));
        assert!(!store.is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile));

        // Complete non-empty det/rec/dict directory imports cleanly.
        fs::write(src_dir.path().join(DICT_FILENAME), b"a\nb\nc\n")?;
        store.import_offline_dir(OcrLanguagePack::CjkEn, OcrModelTier::Mobile, src_dir.path())?;

        assert!(store.is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile));
        let paths = store.pack_paths(OcrLanguagePack::CjkEn, OcrModelTier::Mobile);
        assert_eq!(fs::read(&paths.det)?, b"fake-det-bytes");
        assert_eq!(fs::read(&paths.rec)?, b"fake-rec-bytes");
        assert_eq!(fs::read(&paths.dict)?, b"a\nb\nc\n");

        // No leftover `.part` files in destination directory.
        let dest_dir = store.pack_dir(OcrLanguagePack::CjkEn, OcrModelTier::Mobile);
        for entry in fs::read_dir(&dest_dir)? {
            let name = entry?.file_name();
            assert!(
                !name.to_string_lossy().ends_with(".part"),
                "unexpected leftover .part file: {name:?}"
            );
        }

        let manifest = ModelManifest::builtin();
        let statuses = store.list_statuses(&manifest);
        let cjk_status = statuses
            .iter()
            .find(|s| s.pack == OcrLanguagePack::CjkEn && s.tier == OcrModelTier::Mobile);
        assert!(cjk_status.is_some_and(|s| {
            s.installed
                && s.installed_size_bytes
                    == (b"fake-det-bytes".len() + b"fake-rec-bytes".len() + b"a\nb\nc\n".len())
                        as u64
        }));

        Ok(())
    }

    #[test]
    fn offline_import_failure_preserves_existing_installed_pack() -> TestResult {
        let store_dir = TempDir::new()?;
        let valid_src = TempDir::new()?;
        let bad_src = TempDir::new()?;
        let store = ModelStore::new(store_dir.path().to_path_buf());

        fs::write(valid_src.path().join(DET_MODEL_FILENAME), b"VALID_DET")?;
        fs::write(valid_src.path().join(REC_MODEL_FILENAME), b"VALID_REC")?;
        fs::write(valid_src.path().join(DICT_FILENAME), b"VALID_DICT")?;
        store.import_offline_dir(
            OcrLanguagePack::Korean,
            OcrModelTier::Mobile,
            valid_src.path(),
        )?;

        // Attempt import from incomplete dir.
        fs::write(bad_src.path().join(DET_MODEL_FILENAME), b"BROKEN_DET")?;
        let res = store.import_offline_dir(
            OcrLanguagePack::Korean,
            OcrModelTier::Mobile,
            bad_src.path(),
        );
        assert!(matches!(res, Err(OcrError::InvalidOfflineDir(_))));

        // Original valid pack must remain intact.
        let paths = store.pack_paths(OcrLanguagePack::Korean, OcrModelTier::Mobile);
        assert_eq!(fs::read(&paths.det)?, b"VALID_DET");
        assert_eq!(fs::read(&paths.rec)?, b"VALID_REC");
        assert_eq!(fs::read(&paths.dict)?, b"VALID_DICT");
        Ok(())
    }

    #[test]
    fn delete_pack_removes_installed_files_and_is_idempotent() -> TestResult {
        let store_dir = TempDir::new()?;
        let src_dir = TempDir::new()?;
        let store = ModelStore::new(store_dir.path().to_path_buf());

        fs::write(src_dir.path().join(DET_MODEL_FILENAME), b"det")?;
        fs::write(src_dir.path().join(REC_MODEL_FILENAME), b"rec")?;
        fs::write(src_dir.path().join(DICT_FILENAME), b"dict")?;
        store.import_offline_dir(
            OcrLanguagePack::VietnameseLatin,
            OcrModelTier::Server,
            src_dir.path(),
        )?;
        assert!(store.is_installed(OcrLanguagePack::VietnameseLatin, OcrModelTier::Server));

        store.delete_pack(OcrLanguagePack::VietnameseLatin, OcrModelTier::Server)?;
        assert!(!store.is_installed(OcrLanguagePack::VietnameseLatin, OcrModelTier::Server));

        // Second delete succeeds cleanly.
        store.delete_pack(OcrLanguagePack::VietnameseLatin, OcrModelTier::Server)?;
        Ok(())
    }
}
