// Thumbnail cache: the popup must never decode a full ~5 MB UHD JPEG.
// Thumbnails are generated once per downloaded image (at fetch time) and
// cached under the applet's state dir; the UI only ever loads these.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::ImageError;
use image::imageops::FilterType;

/// Cached thumbnail size (16:9, popup width — matches the reference's
/// preview proportions).
pub const THUMB_WIDTH: u32 = 480;
pub const THUMB_HEIGHT: u32 = 270;

/// Subdirectory of the state dir holding cached thumbnails.
const THUMBS_SUBDIR: &str = "thumbs";

/// Where the cached thumbnail for `image_path` lives:
/// `<state_dir>/thumbs/<same filename>`. The wallpaper filename is unique
/// (`<startdate>-<name>_<res>.jpg`), so reusing it needs no hashing.
pub fn thumbnail_path(image_path: &Path, state_dir: &Path) -> PathBuf {
    state_dir
        .join(THUMBS_SUBDIR)
        .join(image_path.file_name().unwrap_or_default())
}

/// Return the cached thumbnail path for `image_path`, (re)generating it
/// if it is missing or older than the source image. Decodes the full
/// image only when regeneration is needed; writes atomically
/// (`.part` + rename) so a crash never leaves a torn thumbnail.
pub fn ensure_thumbnail(image_path: &Path, state_dir: &Path) -> Result<PathBuf, ImageError> {
    let thumb = thumbnail_path(image_path, state_dir);
    if is_fresh(&thumb, image_path) {
        return Ok(thumb);
    }

    if let Some(parent) = thumb.parent() {
        fs::create_dir_all(parent)?;
    }

    let full = image::open(image_path)?;
    let small = full.resize_to_fill(THUMB_WIDTH, THUMB_HEIGHT, FilterType::Triangle);

    crate::fsutil::write_atomic(&thumb, ".part", |part| {
        small.save_with_format(part, image::ImageFormat::Jpeg)
    })?;
    Ok(thumb)
}

/// Best-effort removal of the cached thumbnails belonging to `images`
/// (called with the paths a prune removed, so the thumbs dir does not
/// grow without bound). A missing thumbnail is fine; any other failure is
/// logged and skipped.
pub fn remove_thumbnails(images: &[PathBuf], state_dir: &Path) {
    for image in images {
        let thumb = thumbnail_path(image, state_dir);
        match fs::remove_file(&thumb) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("failed to remove thumbnail {}: {e}", thumb.display()),
        }
    }
}

/// A thumbnail is fresh when it exists and is at least as new as the
/// source image. Unreadable mtimes count as stale (regenerate — cheap
/// and safe).
fn is_fresh(thumb: &Path, source: &Path) -> bool {
    fn mtime(p: &Path) -> Option<SystemTime> {
        fs::metadata(p).ok()?.modified().ok()
    }
    match (mtime(thumb), mtime(source)) {
        (Some(t), Some(s)) => t >= s,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Write a tiny valid JPEG (not a real UHD asset) for decode tests.
    fn write_test_jpeg(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        img.save_with_format(path, image::ImageFormat::Jpeg)
            .unwrap();
    }

    #[test]
    fn thumbnail_path_mirrors_filename_under_thumbs() {
        assert_eq!(
            thumbnail_path(
                Path::new("/home/u/Pictures/BingWallpaper/20260807-Foo_UHD.jpg"),
                Path::new("/home/u/.local/state/app"),
            ),
            Path::new("/home/u/.local/state/app/thumbs/20260807-Foo_UHD.jpg")
        );
    }

    #[test]
    fn ensure_thumbnail_generates_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        let thumb = ensure_thumbnail(&source, &state).unwrap();

        assert_eq!(thumb, thumbnail_path(&source, &state));
        // Correct size, and readable as an image.
        assert_eq!(
            image::image_dimensions(&thumb).unwrap(),
            (THUMB_WIDTH, THUMB_HEIGHT)
        );
        // No .part leftover.
        assert_eq!(fs::read_dir(state.join(THUMBS_SUBDIR)).unwrap().count(), 1);
    }

    #[test]
    fn ensure_thumbnail_skips_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        // A fresh cache entry is trusted without decoding: garbage content
        // written *after* the source must survive untouched.
        let thumb = thumbnail_path(&source, &state);
        fs::create_dir_all(thumb.parent().unwrap()).unwrap();
        fs::write(&thumb, b"sentinel, not a jpeg").unwrap();

        let got = ensure_thumbnail(&source, &state).unwrap();
        assert_eq!(got, thumb);
        assert_eq!(fs::read(&thumb).unwrap(), b"sentinel, not a jpeg");
    }

    #[test]
    fn ensure_thumbnail_regenerates_when_stale() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("20260807-Foo_UHD.jpg");
        write_test_jpeg(&source, 64, 36);
        let state = dir.path().join("state");

        // Stale cache entry: mtime pushed behind the source's.
        let thumb = thumbnail_path(&source, &state);
        fs::create_dir_all(thumb.parent().unwrap()).unwrap();
        fs::write(&thumb, b"stale garbage").unwrap();
        fs::File::options()
            .write(true)
            .open(&thumb)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();

        let got = ensure_thumbnail(&source, &state).unwrap();
        assert_eq!(
            image::image_dimensions(&got).unwrap(),
            (THUMB_WIDTH, THUMB_HEIGHT)
        );
    }

    #[test]
    fn ensure_thumbnail_missing_source_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.jpg");
        assert!(ensure_thumbnail(&missing, dir.path()).is_err());
    }

    #[test]
    fn remove_thumbnails_deletes_matching_thumbs_and_ignores_missing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let pruned = dir.path().join("20260801-Gone_ROW1_UHD.jpg");
        let kept = dir.path().join("20260807-Kept_ROW2_UHD.jpg");
        let pruned_thumb = thumbnail_path(&pruned, &state);
        let kept_thumb = thumbnail_path(&kept, &state);
        fs::create_dir_all(pruned_thumb.parent().unwrap()).unwrap();
        fs::write(&pruned_thumb, b"thumb").unwrap();
        fs::write(&kept_thumb, b"thumb").unwrap();

        // One image without a thumbnail in the list: must not panic.
        let no_thumb = dir.path().join("20260805-NoThumb_ROW3_UHD.jpg");
        remove_thumbnails(&[pruned, no_thumb], &state);

        assert!(!pruned_thumb.exists(), "pruned image's thumb must go");
        assert!(kept_thumb.exists(), "unrelated thumbs must survive");
    }
}
