// Image catalogue: the persistent record of every downloaded wallpaper.
//
// Stored as JSON in the applet's state dir
// (`~/.local/state/io.github.ercling.CosmicBingWallpaper/catalogue.json`;
// path always injected by the caller so tests never touch the real one).
// The catalogue is rebuildable: if the JSON is corrupt or missing, the
// download folder is rescanned by filename pattern (`bing::parse_filename`
// is the deterministic inverse of `bing::image_filename`), so rebuilt
// entries dedupe cleanly against the next fetch — no duplicates, no
// re-downloads. Titles refill on the next fetch merge.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bing;

/// Filename of the persisted catalogue inside the state dir.
pub const CATALOGUE_FILENAME: &str = "catalogue.json";

/// One downloaded wallpaper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageEntry {
    /// Dedupe key, e.g. `/th?id=OHR.ColorfulCop_ROW6097405388`.
    pub urlbase: String,
    /// `YYYYMMDD`.
    pub startdate: String,
    /// `YYYYMMDDHHMM` UTC. Synthesized as `startdate + "0000"` for entries
    /// rebuilt from a folder scan; replaced by Bing's real value on the
    /// next fetch merge.
    pub fullstartdate: String,
    /// Display title derived from `copyright` (Bing's own `title` field is
    /// the literal string `"Info"`). Empty for rebuilt entries until the
    /// next fetch refills it.
    pub title: String,
    /// The `© …` notice (parens stripped).
    pub copyright: String,
    pub copyrightlink: String,
    /// Absolute path of the downloaded file (any `_<res>` suffix).
    pub filename: PathBuf,
}

impl ImageEntry {
    /// Build an entry from a fetched Bing image and the path its file was
    /// downloaded to. Title/copyright are derived via
    /// [`bing::split_copyright`].
    pub fn from_bing(image: &bing::BingImage, filename: PathBuf) -> Self {
        let (title, copyright) = bing::split_copyright(&image.copyright);
        Self {
            urlbase: image.urlbase.clone(),
            startdate: image.startdate.clone(),
            fullstartdate: image.fullstartdate.clone(),
            title,
            copyright,
            copyrightlink: image.copyrightlink.clone(),
            filename,
        }
    }

    /// A rebuilt entry carries no metadata until the next fetch merge.
    fn is_rebuilt(&self) -> bool {
        self.title.is_empty() && self.copyright.is_empty()
    }

    /// The entry's start time, if its `fullstartdate` parses.
    fn start_time(&self) -> Option<DateTime<Utc>> {
        NaiveDateTime::parse_from_str(&self.fullstartdate, "%Y%m%d%H%M")
            .ok()
            .map(|n| n.and_utc())
    }
}

/// All known images, sorted ascending by `fullstartdate` (oldest first,
/// newest last).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalogue {
    pub images: Vec<ImageEntry>,
}

impl Catalogue {
    /// Load the catalogue JSON from `path`. Missing file and corrupt JSON
    /// are both errors — callers that want the rebuild fallback use
    /// [`Catalogue::load_or_rebuild`].
    pub fn load(path: &Path) -> io::Result<Self> {
        let json = fs::read_to_string(path)?;
        let mut cat: Self = serde_json::from_str(&json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        cat.sort();
        Ok(cat)
    }

    /// Load from `path`, falling back to a rescan of `images_dir` when the
    /// JSON is missing or corrupt (the catalogue is rebuildable by design).
    pub fn load_or_rebuild(path: &Path, images_dir: &Path) -> Self {
        match Self::load(path) {
            Ok(cat) => cat,
            Err(e) => {
                tracing::warn!(
                    "catalogue at {} unusable ({e}); rebuilding from {}",
                    path.display(),
                    images_dir.display()
                );
                Self::rebuild_from_folder(images_dir)
            }
        }
    }

    /// Persist as JSON at `path` atomically (`.tmp` + rename — a crash
    /// never leaves a torn catalogue). Creates parent directories.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)
    }

    /// Rebuild by scanning `dir` for wallpaper files
    /// (`<8 digits>-<name>_<res>.jpg`, any resolution suffix — matches the
    /// reference extension's own migration regex, `utils.js:477`).
    /// Entries get empty titles (refilled on next fetch merge) and a
    /// `fullstartdate` synthesized as `startdate + "0000"`. A missing or
    /// unreadable dir yields an empty catalogue.
    pub fn rebuild_from_folder(dir: &Path) -> Self {
        let mut cat = Self::default();
        let Ok(read) = fs::read_dir(dir) else {
            return cat;
        };
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((startdate, urlbase)) = bing::parse_filename(name) else {
                continue;
            };
            if !entry.path().is_file() {
                continue;
            }
            cat.images.push(ImageEntry {
                urlbase,
                fullstartdate: format!("{startdate}0000"),
                startdate,
                title: String::new(),
                copyright: String::new(),
                copyrightlink: String::new(),
                filename: entry.path(),
            });
        }
        cat.sort();
        cat
    }

    /// Merge freshly fetched entries in, deduping by `urlbase`. When a
    /// fetched entry matches a rebuilt one, the missing metadata (title,
    /// copyright, link, real `fullstartdate`) is filled in while the
    /// existing `filename` is kept — the file on disk (possibly at a
    /// different resolution suffix) stays authoritative, so nothing is
    /// re-downloaded. Result stays sorted ascending by `fullstartdate`.
    pub fn merge(&mut self, new_entries: Vec<ImageEntry>) {
        for incoming in new_entries {
            match self
                .images
                .iter_mut()
                .find(|e| e.urlbase == incoming.urlbase)
            {
                Some(existing) => {
                    if existing.is_rebuilt() {
                        existing.title = incoming.title;
                        existing.copyright = incoming.copyright;
                        existing.copyrightlink = incoming.copyrightlink;
                        existing.fullstartdate = incoming.fullstartdate;
                        // `filename` deliberately kept: the already
                        // downloaded file wins.
                    }
                }
                None => self.images.push(incoming),
            }
        }
        self.sort();
    }

    /// Prune old images (reference semantics, `utils.js:546-550`): delete
    /// files and entries whose `fullstartdate` is older than
    /// `now - retention_days`. `retention_days == 0` means keep forever.
    /// The `currently_applied` file is never deleted. Entries whose file
    /// vanished externally are dropped (nothing to delete). Returns the
    /// paths actually deleted.
    pub fn prune(
        &mut self,
        retention_days: u16,
        currently_applied: Option<&Path>,
        now: DateTime<Utc>,
    ) -> Vec<PathBuf> {
        let cutoff = (retention_days > 0).then(|| now - Duration::days(i64::from(retention_days)));
        let mut deleted = Vec::new();
        self.images.retain(|entry| {
            if !entry.filename.is_file() {
                return false; // vanished externally — drop the entry
            }
            let Some(cutoff) = cutoff else {
                return true; // keep forever
            };
            // Malformed dates are kept — never delete on a guess.
            let too_old = entry.start_time().is_some_and(|t| t < cutoff);
            if !too_old || currently_applied == Some(entry.filename.as_path()) {
                return true;
            }
            match fs::remove_file(&entry.filename) {
                Ok(()) => {
                    deleted.push(entry.filename.clone());
                    false
                }
                Err(e) => {
                    tracing::warn!("failed to prune {}: {e}", entry.filename.display());
                    false // entry goes; file cleanup retried never (best effort)
                }
            }
        });
        deleted
    }

    /// Newest image (last in ascending order).
    pub fn newest(&self) -> Option<&ImageEntry> {
        self.images.last()
    }

    /// Whether `path` is one of the catalogue's downloaded files.
    pub fn contains(&self, path: &Path) -> bool {
        self.position(path).is_some()
    }

    /// The image just older than `current` (the file currently applied);
    /// `None` at the oldest end or when `current` is not in the catalogue.
    pub fn prev(&self, current: &Path) -> Option<&ImageEntry> {
        let i = self.position(current)?;
        self.images.get(i.checked_sub(1)?)
    }

    /// The image just newer than `current`; `None` at the newest end or
    /// when `current` is not in the catalogue.
    pub fn next(&self, current: &Path) -> Option<&ImageEntry> {
        let i = self.position(current)?;
        self.images.get(i + 1)
    }

    /// A pseudo-randomly picked image other than `current` (for shuffle).
    /// `None` when fewer than two images exist. `current` is excluded
    /// structurally (candidates are filtered first), so it can never be
    /// returned regardless of the entropy source.
    pub fn random_other(&self, current: Option<&Path>) -> Option<&ImageEntry> {
        if self.images.len() < 2 {
            return None;
        }
        let candidates: Vec<&ImageEntry> = self
            .images
            .iter()
            .filter(|e| Some(e.filename.as_path()) != current)
            .collect();
        // Wallpaper shuffle needs no cryptographic randomness; clock
        // subsecond nanos avoid pulling in a rand dependency.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0) as usize;
        candidates.get(nanos % candidates.len()).copied()
    }

    fn position(&self, current: &Path) -> Option<usize> {
        self.images.iter().position(|e| e.filename == current)
    }

    /// Sort ascending by `fullstartdate` (ties broken by `urlbase` for
    /// determinism).
    fn sort(&mut self) {
        self.images.sort_by(|a, b| {
            (a.fullstartdate.as_str(), a.urlbase.as_str())
                .cmp(&(b.fullstartdate.as_str(), b.urlbase.as_str()))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Minimal entry with a real file on disk under `dir`.
    fn entry_with_file(dir: &Path, startdate: &str, name: &str) -> ImageEntry {
        let filename = dir.join(bing::image_filename(startdate, &urlbase(name)));
        fs::write(&filename, b"jpeg bytes").unwrap();
        ImageEntry {
            urlbase: urlbase(name),
            startdate: startdate.to_owned(),
            fullstartdate: format!("{startdate}0700"),
            title: format!("Title {name}"),
            copyright: "© Someone".to_owned(),
            copyrightlink: "https://example.com".to_owned(),
            filename,
        }
    }

    fn urlbase(name: &str) -> String {
        format!("/th?id=OHR.{name}")
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(CATALOGUE_FILENAME);
        let cat = Catalogue {
            images: vec![
                entry_with_file(dir.path(), "20260806", "Old_ROW1"),
                entry_with_file(dir.path(), "20260807", "New_ROW2"),
            ],
        };

        cat.save(&path).unwrap();
        let loaded = Catalogue::load(&path).unwrap();

        assert_eq!(loaded, cat);
        // Atomic write leaves no .tmp behind.
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn load_missing_and_corrupt_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CATALOGUE_FILENAME);
        assert!(Catalogue::load(&path).is_err()); // missing

        fs::write(&path, "{ not json !!!").unwrap();
        assert!(Catalogue::load(&path).is_err()); // corrupt

        fs::write(&path, "{\"images\": [{}]}").unwrap();
        assert!(Catalogue::load(&path).is_err()); // wrong shape
    }

    #[test]
    fn corrupt_json_takes_the_rebuild_path() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("BingWallpaper");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("20260806-Foo_ROW1_UHD.jpg"), b"x").unwrap();
        let path = dir.path().join(CATALOGUE_FILENAME);
        fs::write(&path, "corrupt").unwrap();

        let cat = Catalogue::load_or_rebuild(&path, &images);

        assert_eq!(cat.images.len(), 1);
        assert_eq!(cat.images[0].urlbase, urlbase("Foo_ROW1"));
        assert!(cat.images[0].title.is_empty());
    }

    #[test]
    fn rebuild_scans_any_resolution_and_skips_noise() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("20260805-Foo_ROW1_UHD.jpg"), b"x").unwrap();
        fs::write(dir.path().join("20260806-Bar_ROW2_1920x1080.jpg"), b"x").unwrap();
        fs::write(dir.path().join("20260807-Baz_ROW3_1080p.jpg"), b"x").unwrap();
        fs::write(dir.path().join("catalogue.json"), b"not an image").unwrap();
        fs::write(dir.path().join("vacation.jpg"), b"not bing").unwrap();
        fs::write(dir.path().join("20260807-Torn_ROW4_UHD.jpg.part"), b"x").unwrap();

        let cat = Catalogue::rebuild_from_folder(dir.path());

        let urlbases: Vec<&str> = cat.images.iter().map(|e| e.urlbase.as_str()).collect();
        assert_eq!(
            urlbases,
            [
                &urlbase("Foo_ROW1"),
                &urlbase("Bar_ROW2"),
                &urlbase("Baz_ROW3")
            ]
        );
        // Synthesized fullstartdate, empty metadata, absolute filenames.
        assert_eq!(cat.images[0].fullstartdate, "202608050000");
        assert!(cat.images.iter().all(|e| e.title.is_empty()));
        assert!(cat.images.iter().all(|e| e.filename.is_absolute()));
    }

    #[test]
    fn rebuild_of_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalogue::rebuild_from_folder(&dir.path().join("nope"));
        assert!(cat.images.is_empty());
    }

    #[test]
    fn merge_dedupes_and_sorts_ascending() {
        let dir = tempfile::tempdir().unwrap();
        let a = entry_with_file(dir.path(), "20260805", "A_ROW1");
        let b = entry_with_file(dir.path(), "20260806", "B_ROW2");
        let c = entry_with_file(dir.path(), "20260807", "C_ROW3");

        let mut cat = Catalogue {
            images: vec![a.clone(), b.clone()],
        };
        // Incoming newest-first (Bing's order) and overlapping with existing.
        cat.merge(vec![c.clone(), b.clone(), a.clone()]);

        assert_eq!(cat.images, vec![a, b, c]); // deduped, oldest first
    }

    #[test]
    fn merge_fills_rebuilt_entry_without_redownload() {
        let dir = tempfile::tempdir().unwrap();
        // Folder written by the reference extension at 1920x1080.
        fs::write(dir.path().join("20260807-Foo_ROW1_1920x1080.jpg"), b"x").unwrap();
        let mut cat = Catalogue::rebuild_from_folder(dir.path());
        let rebuilt_file = cat.images[0].filename.clone();
        assert_eq!(cat.images[0].fullstartdate, "202608070000");

        // The same image arrives from a fresh fetch (with real metadata).
        let fetched = ImageEntry::from_bing(
            &bing::parse_image_list(
                r#"{"images":[{"urlbase":"/th?id=OHR.Foo_ROW1","startdate":"20260807",
                    "fullstartdate":"202608070700","copyrightlink":"https://example.com",
                    "copyright":"Foo place (© Bar/Getty Images)"}]}"#,
            )
            .unwrap()
            .images[0],
            dir.path().join("20260807-Foo_ROW1_UHD.jpg"),
        );
        cat.merge(vec![fetched]);

        // One entry, metadata refilled, existing file kept (no re-download).
        assert_eq!(cat.images.len(), 1);
        let e = &cat.images[0];
        assert_eq!(e.title, "Foo place");
        assert_eq!(e.copyright, "© Bar/Getty Images");
        assert_eq!(e.fullstartdate, "202608070700");
        assert_eq!(e.filename, rebuilt_file);
        assert!(e.filename.is_file());
    }

    #[test]
    fn merge_never_overwrites_real_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let real = entry_with_file(dir.path(), "20260807", "Foo_ROW1");
        let mut cat = Catalogue {
            images: vec![real.clone()],
        };

        let mut imposter = real.clone();
        imposter.title = "Different title".to_owned();
        imposter.filename = dir.path().join("elsewhere.jpg");
        cat.merge(vec![imposter]);

        assert_eq!(cat.images, vec![real]);
    }

    #[test]
    fn from_bing_derives_title_from_copyright() {
        let image = &bing::parse_image_list(
            r#"{"images":[{"urlbase":"/th?id=OHR.Foo_ROW1","startdate":"20260807",
                "fullstartdate":"202608070700","copyrightlink":"https://example.com",
                "copyright":"Nyhavn Canal, Copenhagen (© emicristea/Getty Images)"}]}"#,
        )
        .unwrap()
        .images[0]
            .clone();

        let entry = ImageEntry::from_bing(image, PathBuf::from("/x/f.jpg"));

        assert_eq!(entry.title, "Nyhavn Canal, Copenhagen");
        assert_eq!(entry.copyright, "© emicristea/Getty Images");
        assert_eq!(entry.urlbase, "/th?id=OHR.Foo_ROW1");
        assert_eq!(entry.filename, PathBuf::from("/x/f.jpg"));
    }

    #[test]
    fn prune_deletes_beyond_cutoff_by_fullstartdate() {
        let dir = tempfile::tempdir().unwrap();
        // now = 2026-08-07 12:00 UTC, retention 3 days → cutoff 2026-08-04 12:00.
        let old = entry_with_file(dir.path(), "20260803", "Old_ROW1"); // 03 07:00 < cutoff
        let edge = entry_with_file(dir.path(), "20260804", "Edge_ROW2"); // 04 07:00 < cutoff
        let mut kept = entry_with_file(dir.path(), "20260804", "Kept_ROW3");
        kept.fullstartdate = "202608041300".to_owned(); // 04 13:00 > cutoff
        let new = entry_with_file(dir.path(), "20260807", "New_ROW4");
        let mut cat = Catalogue {
            images: vec![old.clone(), edge.clone(), kept.clone(), new.clone()],
        };

        let deleted = cat.prune(3, None, now());

        assert_eq!(deleted, vec![old.filename.clone(), edge.filename.clone()]);
        assert!(!old.filename.exists());
        assert!(!edge.filename.exists());
        assert_eq!(cat.images, vec![kept.clone(), new.clone()]);
        assert!(kept.filename.exists());
        assert!(new.filename.exists());
    }

    #[test]
    fn prune_zero_keeps_forever() {
        let dir = tempfile::tempdir().unwrap();
        let ancient = entry_with_file(dir.path(), "19990101", "Ancient_ROW1");
        let mut cat = Catalogue {
            images: vec![ancient.clone()],
        };

        let deleted = cat.prune(0, None, now());

        assert!(deleted.is_empty());
        assert_eq!(cat.images, vec![ancient.clone()]);
        assert!(ancient.filename.exists());
    }

    #[test]
    fn prune_protects_currently_applied() {
        let dir = tempfile::tempdir().unwrap();
        let old_applied = entry_with_file(dir.path(), "20260701", "Applied_ROW1");
        let old_other = entry_with_file(dir.path(), "20260701", "Other_ROW2");
        let mut cat = Catalogue {
            images: vec![old_applied.clone(), old_other.clone()],
        };

        let deleted = cat.prune(3, Some(&old_applied.filename), now());

        assert_eq!(deleted, vec![old_other.filename.clone()]);
        assert!(old_applied.filename.exists());
        assert_eq!(cat.images, vec![old_applied]);
    }

    #[test]
    fn prune_drops_entries_whose_file_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let gone = entry_with_file(dir.path(), "20260807", "Gone_ROW1");
        fs::remove_file(&gone.filename).unwrap();
        let there = entry_with_file(dir.path(), "20260807", "There_ROW2");
        let mut cat = Catalogue {
            images: vec![gone, there.clone()],
        };

        // Even with retention "forever", vanished entries are dropped.
        let deleted = cat.prune(0, None, now());

        assert!(deleted.is_empty()); // nothing was deleted *by us*
        assert_eq!(cat.images, vec![there]);
    }

    #[test]
    fn prune_keeps_malformed_dates() {
        let dir = tempfile::tempdir().unwrap();
        let mut odd = entry_with_file(dir.path(), "20260101", "Odd_ROW1");
        odd.fullstartdate = "not-a-date".to_owned();
        let mut cat = Catalogue {
            images: vec![odd.clone()],
        };

        cat.prune(3, None, now());

        assert_eq!(cat.images, vec![odd]); // never delete on a guess
    }

    #[test]
    fn navigation_on_empty_catalogue() {
        let cat = Catalogue::default();
        assert!(cat.newest().is_none());
        assert!(cat.prev(Path::new("/x.jpg")).is_none());
        assert!(cat.next(Path::new("/x.jpg")).is_none());
        assert!(cat.random_other(None).is_none());
    }

    #[test]
    fn navigation_single_image() {
        let dir = tempfile::tempdir().unwrap();
        let only = entry_with_file(dir.path(), "20260807", "Only_ROW1");
        let cat = Catalogue {
            images: vec![only.clone()],
        };

        assert_eq!(cat.newest(), Some(&only));
        assert!(cat.prev(&only.filename).is_none());
        assert!(cat.next(&only.filename).is_none());
        assert!(cat.random_other(Some(&only.filename)).is_none()); // n < 2
    }

    #[test]
    fn navigation_walks_both_directions_and_stops_at_ends() {
        let dir = tempfile::tempdir().unwrap();
        let a = entry_with_file(dir.path(), "20260805", "A_ROW1");
        let b = entry_with_file(dir.path(), "20260806", "B_ROW2");
        let c = entry_with_file(dir.path(), "20260807", "C_ROW3");
        let cat = Catalogue {
            images: vec![a.clone(), b.clone(), c.clone()],
        };

        assert_eq!(cat.newest(), Some(&c));
        assert_eq!(cat.prev(&b.filename), Some(&a));
        assert_eq!(cat.next(&b.filename), Some(&c));
        assert!(cat.prev(&a.filename).is_none()); // oldest end
        assert!(cat.next(&c.filename).is_none()); // newest end
        // Unknown current file: nothing to navigate from.
        assert!(cat.prev(Path::new("/unknown.jpg")).is_none());
        assert!(cat.next(Path::new("/unknown.jpg")).is_none());
    }

    #[test]
    fn random_other_never_returns_current() {
        let dir = tempfile::tempdir().unwrap();
        let a = entry_with_file(dir.path(), "20260805", "A_ROW1");
        let b = entry_with_file(dir.path(), "20260806", "B_ROW2");
        let c = entry_with_file(dir.path(), "20260807", "C_ROW3");
        let cat = Catalogue {
            images: vec![a.clone(), b.clone(), c.clone()],
        };

        for _ in 0..200 {
            let picked = cat.random_other(Some(&b.filename)).unwrap();
            assert_ne!(picked.filename, b.filename);
        }
        // With no current, any image qualifies — but it must pick one.
        assert!(cat.random_other(None).is_some());
    }
}
