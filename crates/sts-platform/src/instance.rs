//! Cross-platform single-instance guard backed by an advisory OS file lock.

use std::fs::{self, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::PlatformError;

/// Holds an exclusive advisory OS file lock for the lifetime of the primary process.
#[derive(Debug)]
pub struct SingleInstanceGuard {
    _file: std::fs::File,
    path: PathBuf,
}

impl SingleInstanceGuard {
    /// Attempts to acquire an exclusive lock at `path` without blocking.
    ///
    /// Returns `Ok(Some(guard))` when this process is the primary instance, `Ok(None)` when
    /// another active process already holds the lock, or [`PlatformError::SingleInstanceFailed`] on
    /// filesystem/OS errors.
    pub fn try_acquire_at(path: impl Into<PathBuf>) -> Result<Option<Self>, PlatformError> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|err| {
                PlatformError::SingleInstanceFailed(format!(
                    "failed to create lock directory `{}`: {err}",
                    parent.display()
                ))
            })?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                PlatformError::SingleInstanceFailed(format!(
                    "failed to open lock file `{}`: {err}",
                    path.display()
                ))
            })?;

        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(PlatformError::SingleInstanceFailed(format!(
                "failed to lock `{}`: {err}",
                path.display()
            ))),
        }
    }

    /// Returns the filesystem path of the acquired lock file.
    #[must_use]
    pub fn lock_path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn single_instance_guard_blocks_second_acquire_and_allows_reacquire_after_drop()
    -> Result<(), PlatformError> {
        let dir =
            TempDir::new().map_err(|err| PlatformError::SingleInstanceFailed(err.to_string()))?;
        let lock_path = dir.path().join("square-to-search.lock");

        let guard1 = SingleInstanceGuard::try_acquire_at(&lock_path)?;
        assert!(guard1.is_some(), "first acquire should succeed");
        let guard1 = match guard1 {
            Some(g) => g,
            None => {
                return Err(PlatformError::SingleInstanceFailed(String::from(
                    "expected first lock acquire to return Some",
                )));
            }
        };
        assert_eq!(guard1.lock_path(), lock_path.as_path());

        let guard2 = SingleInstanceGuard::try_acquire_at(&lock_path)?;
        assert!(
            guard2.is_none(),
            "second acquire while primary holds guard must return Ok(None)"
        );

        drop(guard1);

        let guard3 = SingleInstanceGuard::try_acquire_at(&lock_path)?;
        assert!(
            guard3.is_some(),
            "lock must be acquirable again after dropping primary guard"
        );
        Ok(())
    }

    #[test]
    fn single_instance_guard_creates_missing_parent_directories() -> Result<(), PlatformError> {
        let dir =
            TempDir::new().map_err(|err| PlatformError::SingleInstanceFailed(err.to_string()))?;
        let nested_lock = dir
            .path()
            .join("nested")
            .join("config")
            .join("square-to-search.lock");

        let guard = SingleInstanceGuard::try_acquire_at(&nested_lock)?;
        assert!(guard.is_some());
        assert!(nested_lock.exists());
        Ok(())
    }
}
