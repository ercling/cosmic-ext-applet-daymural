// Shared atomic-write mechanics: downloaded images, the catalogue JSON,
// and cached thumbnails are all written to a temporary sibling first and
// renamed into place, so a crash never leaves a torn file at the final
// path.

use std::io;
use std::path::{Path, PathBuf};

/// Temporary-sibling suffix for a *partially fetched* file: downloaded
/// images and cached thumbnails. Orphans carrying it are swept by
/// `bing::sweep_part_files` and `thumbs::reconcile`.
pub const PART_SUFFIX: &str = ".part";

/// Temporary-sibling suffix for a whole-file rewrite of something already
/// on disk (the catalogue JSON) — nothing "partial" ever existed at the
/// destination, so it reads as a temp file rather than a fragment.
pub const TMP_SUFFIX: &str = ".tmp";

/// `<dest><suffix>` — the temporary sibling path a write goes to before
/// the atomic rename.
pub fn temp_sibling(dest: &Path, suffix: &str) -> PathBuf {
    let mut os = dest.as_os_str().to_owned();
    os.push(suffix);
    PathBuf::from(os)
}

/// Write `dest` atomically: `write` produces the file at the temporary
/// `<dest><suffix>` sibling, which is then renamed over `dest` (atomic on
/// the same filesystem — no torn files). Works with any error type that
/// can carry the rename's `io::Error`.
pub fn write_atomic<E: From<io::Error>>(
    dest: &Path,
    suffix: &str,
    write: impl FnOnce(&Path) -> Result<(), E>,
) -> Result<(), E> {
    let temp = temp_sibling(dest, suffix);
    write(&temp)?;
    std::fs::rename(&temp, dest).map_err(E::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_sibling_appends_suffix() {
        assert_eq!(
            temp_sibling(Path::new("/x/20260807-Foo_UHD.jpg"), PART_SUFFIX),
            Path::new("/x/20260807-Foo_UHD.jpg.part")
        );
        assert_eq!(
            temp_sibling(Path::new("/x/catalogue.json"), TMP_SUFFIX),
            Path::new("/x/catalogue.json.tmp")
        );
    }

    #[test]
    fn write_atomic_renames_temp_to_final() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("20260807-Foo_UHD.jpg");

        write_atomic::<io::Error>(&dest, PART_SUFFIX, |p| std::fs::write(p, b"jpeg bytes"))
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"jpeg bytes");
        assert!(
            !temp_sibling(&dest, PART_SUFFIX).exists(),
            ".part must not survive"
        );
        // Only the final file remains in the dir.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn write_atomic_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.jpg");
        std::fs::write(&dest, b"old").unwrap();
        write_atomic::<io::Error>(&dest, TMP_SUFFIX, |p| std::fs::write(p, b"new")).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }

    #[test]
    fn write_atomic_failed_write_leaves_dest_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.jpg");
        std::fs::write(&dest, b"old").unwrap();

        let err = write_atomic(&dest, PART_SUFFIX, |_| {
            Err::<(), io::Error>(io::Error::other("encode failed"))
        })
        .unwrap_err();

        assert_eq!(err.to_string(), "encode failed");
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    }
}
