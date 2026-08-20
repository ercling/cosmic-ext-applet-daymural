//! Cross-process leadership and coordination locks.
//!
//! COSMIC starts one copy of the applet for each output.  The leader lock
//! elects the one copy allowed to perform shared background work, while the
//! shorter-lived coordination lock serializes mailbox read-modify-write
//! operations.  Both locks are advisory and are released when their open file
//! is dropped, including during unwinding or process exit.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

const LEADER_LOCK: &str = "leader.lock";
const COORDINATION_LOCK: &str = "coordination.lock";

#[derive(Debug)]
pub(crate) struct Leadership {
    leader: bool,
    // Keep the descriptor open for either result: winners retain the lock and
    // losers retry locking this same handle when their timer fires.
    file: Option<File>,
}

impl Leadership {
    pub(crate) fn acquire(dir: &Path) -> Self {
        let path = dir.join(LEADER_LOCK);
        let file = match open_lock_file(dir, &path) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    "cannot open applet leader lock; failing open as leader: {error}"
                );
                return Self::default();
            }
        };

        match file.try_lock() {
            Ok(()) => Self {
                leader: true,
                file: Some(file),
            },
            Err(TryLockError::WouldBlock) => Self {
                leader: false,
                file: Some(file),
            },
            Err(TryLockError::Error(error)) => {
                tracing::warn!(
                    path = %path.display(),
                    "cannot acquire applet leader lock; failing open as leader: {error}"
                );
                Self {
                    leader: true,
                    file: Some(file),
                }
            }
        }
    }

    pub(crate) fn is_leader(&self) -> bool {
        self.leader
    }

    /// Retry leadership on the handle retained by a lock loser.
    ///
    /// Returns `true` exactly once, on the transition from follower to leader.
    pub(crate) fn try_acquire(&mut self) -> bool {
        if self.leader {
            return false;
        }

        let Some(file) = self.file.as_ref() else {
            // Only test-forced followers lack a handle. Production followers
            // always retain the successfully opened leader-lock file.
            return false;
        };

        match file.try_lock() {
            Ok(()) => {
                self.leader = true;
                true
            }
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(error)) => {
                tracing::warn!("cannot retry applet leader lock; failing open as leader: {error}");
                self.leader = true;
                true
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn forced(leader: bool) -> Self {
        Self { leader, file: None }
    }
}

impl Default for Leadership {
    fn default() -> Self {
        Self {
            leader: true,
            file: None,
        }
    }
}

/// Run one coordination mailbox operation under its cross-process lock.
///
/// This performs blocking filesystem work and must therefore only be called
/// from the blocking pool in production. Dropping `file` releases the lock on
/// normal return, closure error, unwind, or process exit.
pub(crate) fn with_coordination_lock<T, E>(
    dir: &Path,
    operation: impl FnOnce() -> Result<T, E>,
) -> Result<T, E>
where
    E: From<io::Error>,
{
    let path = dir.join(COORDINATION_LOCK);
    let file = open_lock_file(dir, &path)?;
    file.lock()?;
    operation()
}

fn open_lock_file(dir: &Path, path: &Path) -> io::Result<File> {
    std::fs::create_dir_all(dir)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn one_leader_wins_and_loser_takes_over_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = Leadership::acquire(dir.path());
        let mut second = Leadership::acquire(dir.path());

        assert!(first.is_leader());
        assert!(!second.is_leader());
        assert!(!second.try_acquire(), "winner still holds the lock");

        drop(first);
        assert!(second.try_acquire(), "loser acquires after winner drops");
        assert!(second.is_leader());
        assert!(
            !second.try_acquire(),
            "the acquisition edge fires only once"
        );
    }

    #[test]
    fn forced_and_default_roles_are_deterministic() {
        assert!(Leadership::default().is_leader());
        assert!(Leadership::forced(true).is_leader());

        let mut follower = Leadership::forced(false);
        assert!(!follower.is_leader());
        assert!(!follower.try_acquire());
        assert!(!follower.is_leader());
    }

    #[test]
    fn leader_lock_open_error_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(LEADER_LOCK)).unwrap();

        let leadership = Leadership::acquire(dir.path());

        assert!(leadership.is_leader());
    }

    #[test]
    fn coordination_critical_sections_serialize() {
        const THREADS: usize = 6;
        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(THREADS));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let barrier = Arc::clone(&barrier);
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                let path = dir.path().to_owned();
                scope.spawn(move || {
                    barrier.wait();
                    with_coordination_lock(&path, || -> io::Result<()> {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(10));
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });

        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn coordination_lock_is_released_after_closure_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = with_coordination_lock(dir.path(), || {
            Err::<(), io::Error>(io::Error::other("mailbox write failed"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "mailbox write failed");

        with_coordination_lock(dir.path(), || Ok::<_, io::Error>(())).unwrap();
    }

    #[test]
    fn coordination_lock_is_released_after_unwind() {
        let dir = tempfile::tempdir().unwrap();
        let result = std::panic::catch_unwind(|| {
            let _: io::Result<()> = with_coordination_lock(dir.path(), || {
                panic!("simulated mailbox panic");
            });
        });
        assert!(result.is_err());

        with_coordination_lock(dir.path(), || Ok::<_, io::Error>(())).unwrap();
    }
}
