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
use crate::fsutil::{self, TMP_SUFFIX};

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

    /// The entry's `filename` legitimately names the entry's *own* image:
    /// its basename is a wallpaper filename whose `<name>` part matches
    /// `urlbase` ([`bing::filename_names_urlbase`]). The catalogue JSON is
    /// user-editable state, so an entry is never trusted to act on a file
    /// it does not name — a tampered entry pointing at a *different*
    /// image's valid file must neither have prune delete that file nor
    /// have `existing_file` skip its own image's download.
    pub fn names_own_file(&self) -> bool {
        self.filename
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| bing::filename_names_urlbase(n, &self.urlbase))
    }

    /// The entry's start time, if its `fullstartdate` parses.
    fn start_time(&self) -> Option<DateTime<Utc>> {
        NaiveDateTime::parse_from_str(&self.fullstartdate, "%Y%m%d%H%M")
            .ok()
            .map(|n| n.and_utc())
    }

    /// Whether the entry is young enough to survive a prune with
    /// `retention_days` at `now` (`0` = keep forever). Malformed dates count
    /// as young — never delete on a guess.
    ///
    /// [`Catalogue::prune`] is the primary caller, but the age test is public
    /// so everything that wants to know what the *next* prune will delete
    /// asks the same question: the thumbnail backfill in `app.rs` skips
    /// entries this rejects rather than decoding ~5 MB apiece for thumbnails
    /// the same refresh unlinks minutes later. Two spellings of the cutoff
    /// would drift.
    pub fn within_retention(&self, retention_days: u16, now: DateTime<Utc>) -> bool {
        if retention_days == 0 {
            return true;
        }
        let cutoff = now - Duration::days(i64::from(retention_days));
        !self.start_time().is_some_and(|t| t < cutoff)
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
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        cat.sort();
        Ok(cat)
    }

    /// Load from `path`, falling back to a rescan of `images_dir` when the
    /// JSON is missing, corrupt, or *empty* (the catalogue is rebuildable by
    /// design).
    ///
    /// An empty catalogue is treated exactly like an unusable one: it holds
    /// no more information than a fresh install does, while the folder it
    /// describes may be full of images. Without the rescan, a single bad
    /// startup that persisted an empty catalogue (a download folder that was
    /// briefly unreachable — see [`Catalogue::prune`]) would be permanent:
    /// valid-but-empty JSON loads fine, so nothing would ever rescan again
    /// and every restored image would stay invisible and unpruned forever.
    pub fn load_or_rebuild(path: &Path, images_dir: &Path) -> Self {
        match Self::load(path) {
            Ok(cat) if !cat.images.is_empty() => cat,
            Ok(_) => {
                tracing::info!(
                    "catalogue at {} is empty; rescanning {}",
                    path.display(),
                    images_dir.display()
                );
                Self::rebuild_from_folder(images_dir)
            }
            Err(error) => {
                tracing::warn!(
                    "catalogue at {} unusable ({error}); rebuilding from {}",
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
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fsutil::write_atomic(path, TMP_SUFFIX, |tmp| fs::write(tmp, &json))
    }

    /// Rebuild by scanning `dir` for wallpaper files
    /// (`<8 digits>-<name>_<res>.jpg`, any resolution suffix — matches the
    /// reference extension's own migration regex, `utils.js:477`).
    /// Entries get empty titles (refilled on next fetch merge) and a
    /// `fullstartdate` synthesized as `startdate + "0000"`. A missing or
    /// unreadable dir yields an empty catalogue.
    ///
    /// One entry per `urlbase`: a migrated folder may hold the same image
    /// several times — at multiple resolutions (the reference extension's
    /// resolution setting changed over time) or on multiple dates (Bing
    /// repeats images). Duplicate entries would break the catalogue's
    /// dedupe invariant (`merge` and `existing_file` match the *first*
    /// hit). The winner is the greatest `(startdate, filename)` — newest
    /// date first, and for same-date ties `_UHD` beats numeric resolution
    /// suffixes (ASCII `U` > digits), i.e. our own download target. Loser
    /// files stay on disk untracked, like any foreign file in the folder.
    pub fn rebuild_from_folder(dir: &Path) -> Self {
        use std::collections::HashMap;

        let mut cat = Self::default();
        let Ok(read) = fs::read_dir(dir) else {
            return cat;
        };
        let mut best: HashMap<String, ImageEntry> = HashMap::new();
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((startdate, urlbase)) = bing::parse_filename(name) else {
                continue;
            };
            if !entry.path().is_file() {
                continue;
            }
            let candidate = ImageEntry {
                urlbase,
                fullstartdate: format!("{startdate}0000"),
                startdate,
                title: String::new(),
                copyright: String::new(),
                copyrightlink: String::new(),
                filename: entry.path(),
            };
            let key = |e: &ImageEntry| (e.startdate.clone(), e.filename.clone());
            match best.entry(candidate.urlbase.clone()) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    if key(&candidate) > key(slot.get()) {
                        slot.insert(candidate);
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(candidate);
                }
            }
        }
        cat.images = best.into_values().collect();
        cat.sort();
        cat
    }

    /// Merge freshly fetched entries in, deduping by `urlbase`. When a
    /// fetched entry matches a rebuilt one, the missing metadata (title,
    /// copyright, link) is filled in while the existing `filename` is
    /// kept — the file on disk (possibly at a different resolution
    /// suffix) stays authoritative, so nothing is re-downloaded. Result
    /// stays sorted ascending by `fullstartdate`.
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
                        // `filename` deliberately kept: the already
                        // downloaded file wins…
                    }
                    // Bing occasionally re-runs an image under an
                    // unchanged `urlbase` on a later day (the same
                    // assumption `rebuild_from_folder`'s dedupe makes).
                    // Adopt the newer dates — keeping the stale ones
                    // would make `newest()`/auto-apply target the wrong
                    // image and derive the refresh schedule from a date
                    // in the past (a tight retry loop until the date
                    // rolls over). Also refreshes a rebuilt entry's
                    // synthesized `…0000` with Bing's real time.
                    // Fixed-width digit strings: lexical order is
                    // chronological.
                    if incoming.fullstartdate > existing.fullstartdate {
                        existing.fullstartdate = incoming.fullstartdate;
                        existing.startdate = incoming.startdate;
                    }
                    // …unless the entry's file claim is dead or
                    // illegitimate while the incoming entry holds a fresh
                    // download: the file vanished externally, or a
                    // tampered catalogue pointed the entry at a file it
                    // does not name (either way `existing_file` misses
                    // and the pipeline re-downloads) — adopt the new
                    // path instead of orphaning the download.
                    if (!existing.filename.is_file() || !existing.names_own_file())
                        && incoming.filename.is_file()
                    {
                        existing.filename = incoming.filename;
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
    /// vanished externally are dropped (nothing to delete). When deleting
    /// a file *fails*, the entry is kept so the next prune retries —
    /// dropping it would orphan the file forever (a later rebuild would
    /// resurrect it metadata-less). Returns every path removed from the
    /// catalogue — deleted here, found vanished, or rejected as foreign —
    /// so the caller can clean up derived artifacts (cached thumbnails) —
    /// except paths a surviving entry still references (a scrubbed entry
    /// may have pointed at another entry's file; its thumbnail must live).
    ///
    /// The catalogue JSON is user-editable state, so a deserialized path
    /// is never trusted with deletion: only files directly inside
    /// `images_dir` whose name is a Bing wallpaper filename naming the
    /// entry's *own* `urlbase` ([`ImageEntry::names_own_file`]) qualify —
    /// an entry may only delete the file it legitimately names. A
    /// tampered entry pointing anywhere else — outside the dir, at a
    /// non-wallpaper name, or at a *different* image's valid file — is
    /// dropped from the catalogue with its file left untouched, so a
    /// hand-edited `catalogue.json` can never make the prune delete
    /// arbitrary files or another entry's image.
    ///
    /// The whole pass is skipped when `images_dir` cannot be enumerated at
    /// all (never created yet, renamed, on an unmounted or slow-mounting
    /// drive, unreadable modes). "This one file is gone" is only evidence
    /// while the folder holding it is readable — otherwise *every* entry
    /// looks vanished, and dropping them all would discard the catalogue's
    /// entire history for a transient the next start recovers from. Nothing
    /// is lost by skipping: there is no reachable file to delete either.
    pub fn prune(
        &mut self,
        images_dir: &Path,
        retention_days: u16,
        currently_applied: Option<&Path>,
        now: DateTime<Utc>,
    ) -> Vec<PathBuf> {
        self.prune_protecting(
            images_dir,
            retention_days,
            currently_applied.as_slice(),
            now,
        )
    }

    /// [`Catalogue::prune`] with any number of age-exempt files: the
    /// currently applied wallpaper, plus a just-downloaded out-of-retention
    /// fallback that the refresh still has to apply (see `app.rs`'s
    /// `RefreshBatch::fallback`). Exemption is from the *age* rule only — a
    /// protected path whose file vanished or that is foreign to
    /// `images_dir` is dropped exactly like any other entry.
    pub fn prune_protecting(
        &mut self,
        images_dir: &Path,
        retention_days: u16,
        protected: &[&Path],
        now: DateTime<Utc>,
    ) -> Vec<PathBuf> {
        if fs::read_dir(images_dir).is_err() {
            tracing::warn!(
                "skipping prune: {} cannot be read right now",
                images_dir.display()
            );
            return Vec::new();
        }
        let mut removed = Vec::new();
        self.images.retain(|entry| {
            let ours = entry.filename.parent() == Some(images_dir) && entry.names_own_file();
            if !ours {
                tracing::warn!(
                    "dropping catalogue entry with foreign path {} (file left untouched)",
                    entry.filename.display()
                );
                removed.push(entry.filename.clone());
                return false;
            }
            if !entry.filename.is_file() {
                removed.push(entry.filename.clone());
                return false; // vanished externally — drop the entry
            }
            if entry.within_retention(retention_days, now)
                || protected.contains(&entry.filename.as_path())
            {
                return true;
            }
            match fs::remove_file(&entry.filename) {
                Ok(()) => {
                    removed.push(entry.filename.clone());
                    false
                }
                Err(error) => {
                    tracing::warn!("failed to prune {}: {error}", entry.filename.display());
                    true // keep the entry — retry the deletion next prune
                }
            }
        });
        // A scrubbed entry may have pointed at a file another (legitimate)
        // entry still holds — never report a path the catalogue still
        // references, or the caller would delete the survivor's cached
        // thumbnail. Thumbnails are keyed by *basename*
        // ([`crate::thumbs::thumbnail_path`]), so the comparison must be
        // too: a foreign path merely *sharing* a survivor's basename
        // would otherwise take the survivor's thumbnail with it.
        removed.retain(|path| {
            !self
                .images
                .iter()
                .any(|e| e.filename.file_name() == path.file_name())
        });
        removed
    }

    /// Remove the entries Bing has *explicitly* marked ineligible
    /// (`wp: false`) — entry **and** file, so a later
    /// [`Catalogue::rebuild_from_folder`] cannot resurrect the image.
    /// Returns the removed paths, for the thumbnail sweep.
    ///
    /// Mirrors [`Catalogue::prune`] rule for rule: only a file the entry
    /// legitimately names inside `images_dir` is unlinked (a foreign path is
    /// never deleted — but its entry is dropped, as prune does); an entry is
    /// dropped only once its file is confirmed gone (the unlink succeeded or
    /// the file was already absent) — on unlink failure it is kept and the
    /// next refresh retries, so there is never a window where an entry is
    /// gone while a rebuildable JPEG remains; and the currently applied
    /// file is exempt, entry and file, so the popup keeps attributing the
    /// image that is actually on screen (`view::displayed` would otherwise
    /// fall back to the newest entry). It goes on the first refresh after
    /// another image is applied. Callers hold the `CurrentWallpaper::Unknown`
    /// guard (see `app.rs`): when the displayed file is unknowable,
    /// `currently_applied` protects nothing and nothing may be deleted.
    pub fn remove_ineligible(
        &mut self,
        urlbases: &[String],
        images_dir: &Path,
        currently_applied: Option<&Path>,
    ) -> Vec<PathBuf> {
        let mut removed = Vec::new();
        if urlbases.is_empty() {
            return removed;
        }
        self.images.retain(|entry| {
            if !urlbases.contains(&entry.urlbase)
                || currently_applied == Some(entry.filename.as_path())
            {
                return true;
            }
            let ours = entry.filename.parent() == Some(images_dir) && entry.names_own_file();
            if !ours {
                tracing::warn!(
                    "dropping ineligible catalogue entry with foreign path {} (file left untouched)",
                    entry.filename.display()
                );
                removed.push(entry.filename.clone());
                return false;
            }
            match fs::remove_file(&entry.filename) {
                Ok(()) => {
                    tracing::info!(
                        "removed {}: Bing marks it ineligible as a wallpaper",
                        entry.filename.display()
                    );
                    removed.push(entry.filename.clone());
                    false
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    removed.push(entry.filename.clone());
                    false
                }
                Err(error) => {
                    tracing::warn!(
                        "failed to remove ineligible {}: {error}",
                        entry.filename.display()
                    );
                    true // keep the entry — retry on the next refresh
                }
            }
        });
        removed.retain(|path| {
            !self
                .images
                .iter()
                .any(|e| e.filename.file_name() == path.file_name())
        });
        removed
    }

    /// Newest image (last in ascending order).
    pub fn newest(&self) -> Option<&ImageEntry> {
        self.images.last()
    }

    /// Whether `path` is one of the catalogue's downloaded files.
    pub fn contains(&self, path: &Path) -> bool {
        self.position(path).is_some()
    }

    /// The entry whose file is `path`, if any.
    pub fn entry_for(&self, path: &Path) -> Option<&ImageEntry> {
        self.position(path).map(|index| &self.images[index])
    }

    /// The already-downloaded file for `urlbase`, if some entry holds one
    /// that still exists on disk. The fetch pipeline consults this before
    /// downloading: a rebuilt entry may cover the image at a *different*
    /// resolution suffix (e.g. `_1920x1080` from the reference extension),
    /// where the mere existence check on the UHD path would miss it and
    /// re-download ~5 MB the merge then orphans.
    ///
    /// Only files the entry legitimately names *inside `images_dir`* count —
    /// the same containment gate [`Catalogue::prune`] and the thumbnail
    /// backfill apply ([`ImageEntry::names_own_file`] plus the parent
    /// directory). A tampered catalogue pointing an entry at a *different*
    /// image's file, or at a file outside the download folder, must not skip
    /// this image's download — the pipeline re-downloads and the merge then
    /// heals the entry with the fresh path.
    pub fn existing_file(&self, urlbase: &str, images_dir: &Path) -> Option<PathBuf> {
        self.images
            .iter()
            .find(|e| {
                e.urlbase == urlbase
                    && e.filename.parent() == Some(images_dir)
                    && e.names_own_file()
                    && e.filename.is_file()
            })
            .map(|e| e.filename.clone())
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
        // `images.len() >= 2` does not guarantee candidates exist: entries
        // sharing `current`'s filename (only constructible via catalogue
        // tampering — merge dedupes and prune scrubs such entries) would
        // all be filtered out, and the modulo below must never see zero.
        if candidates.is_empty() {
            return None;
        }
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
    fn load_sorts_out_of_order_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CATALOGUE_FILENAME);
        let older = entry_with_file(dir.path(), "20260806", "Old_ROW1");
        let newer = entry_with_file(dir.path(), "20260807", "New_ROW2");
        // Hand-write the file newest-first: load must restore ascending
        // order (navigation and `newest()` rely on it).
        let unsorted = Catalogue {
            images: vec![newer.clone(), older.clone()],
        };
        fs::write(&path, serde_json::to_string(&unsorted).unwrap()).unwrap();

        let loaded = Catalogue::load(&path).unwrap();

        assert_eq!(loaded.images, vec![older, newer]);
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
    fn an_empty_catalogue_takes_the_rebuild_path_too() {
        // Valid-but-empty JSON is the state a single bad startup can persist
        // (a briefly unreachable download folder used to drop every entry).
        // It loads fine, so without treating "empty" as "unusable" nothing
        // would ever rescan and the restored images would stay invisible —
        // and unpruned — for good.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("BingWallpaper");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("20260806-Foo_ROW1_UHD.jpg"), b"x").unwrap();
        let path = dir.path().join(CATALOGUE_FILENAME);
        Catalogue::default().save(&path).unwrap();
        assert!(Catalogue::load(&path).unwrap().images.is_empty());

        let cat = Catalogue::load_or_rebuild(&path, &images);

        assert_eq!(cat.images.len(), 1);
        assert_eq!(cat.images[0].urlbase, urlbase("Foo_ROW1"));

        // A genuinely empty folder still yields an empty catalogue (the
        // cold start must stay armed).
        let empty = dir.path().join("Empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(Catalogue::load_or_rebuild(&path, &empty).images.is_empty());
    }

    #[test]
    fn prune_does_nothing_while_the_images_dir_cannot_be_read() {
        // A folder that is renamed, not mounted yet, or unreadable makes
        // *every* entry look vanished. Dropping them all would discard the
        // whole catalogue for a transient — and the caller persists that
        // result — so the pass is skipped entirely instead.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("BingWallpaper");
        fs::create_dir_all(&images).unwrap();
        let old = entry_with_file(&images, "20260101", "Old_ROW1");
        let new = entry_with_file(&images, "20260807", "New_ROW2");
        let mut cat = Catalogue {
            images: vec![old.clone(), new.clone()],
        };
        fs::rename(&images, dir.path().join("moved")).unwrap();

        // Neither the vanished-entry sweep nor age deletion runs.
        assert!(cat.prune(&images, 3, None, now()).is_empty());
        assert_eq!(cat.images, vec![old.clone(), new.clone()]);

        // Back in place, the ordinary rules apply again.
        fs::rename(dir.path().join("moved"), &images).unwrap();
        assert_eq!(cat.prune(&images, 3, None, now()), vec![old.filename]);
        assert_eq!(cat.images, vec![new]);
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
    fn rebuild_dedupes_by_urlbase() {
        let dir = tempfile::tempdir().unwrap();
        // The same image at two resolutions (reference extension's setting
        // changed over time) — one entry, the UHD file wins the tie.
        fs::write(dir.path().join("20260806-Foo_ROW1_1920x1080.jpg"), b"x").unwrap();
        fs::write(dir.path().join("20260806-Foo_ROW1_UHD.jpg"), b"x").unwrap();
        // The same image repeated by Bing on two dates — the newest wins.
        fs::write(dir.path().join("20240101-Bar_ROW2_UHD.jpg"), b"x").unwrap();
        fs::write(dir.path().join("20260807-Bar_ROW2_UHD.jpg"), b"x").unwrap();

        let cat = Catalogue::rebuild_from_folder(dir.path());

        assert_eq!(cat.images.len(), 2);
        let foo = cat
            .images
            .iter()
            .find(|e| e.urlbase.contains("Foo"))
            .unwrap();
        assert_eq!(foo.filename, dir.path().join("20260806-Foo_ROW1_UHD.jpg"));
        let bar = cat
            .images
            .iter()
            .find(|e| e.urlbase.contains("Bar"))
            .unwrap();
        assert_eq!(bar.startdate, "20260807");
        // A later merge of the same urlbase updates the one entry cleanly.
        assert_eq!(
            cat.existing_file(&urlbase("Foo_ROW1"), dir.path()),
            Some(dir.path().join("20260806-Foo_ROW1_UHD.jpg"))
        );
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
                    "copyright":"Foo place (© Bar/Getty Images)","wp":true}]}"#,
            )
            .unwrap()
            .eligible[0]
                .image,
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
    fn existing_file_finds_rebuilt_files_at_any_resolution() {
        let dir = tempfile::tempdir().unwrap();
        // Folder written by the reference extension at 1920x1080 — the UHD
        // path the pipeline would download to does not exist.
        fs::write(dir.path().join("20260807-Foo_ROW1_1920x1080.jpg"), b"x").unwrap();
        let cat = Catalogue::rebuild_from_folder(dir.path());

        // The pipeline's pre-download check must find the existing file...
        assert_eq!(
            cat.existing_file(&urlbase("Foo_ROW1"), dir.path()),
            Some(dir.path().join("20260807-Foo_ROW1_1920x1080.jpg"))
        );
        // ...and report nothing for images not on disk.
        assert_eq!(cat.existing_file(&urlbase("Other_ROW2"), dir.path()), None);
    }

    #[test]
    fn existing_file_ignores_entries_whose_file_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let entry = entry_with_file(dir.path(), "20260807", "Foo_ROW1");
        let cat = Catalogue {
            images: vec![entry.clone()],
        };
        fs::remove_file(&entry.filename).unwrap();

        // Vanished file → the pipeline should re-download, not trust the
        // stale catalogue path.
        assert_eq!(cat.existing_file(&urlbase("Foo_ROW1"), dir.path()), None);
    }

    #[test]
    fn existing_file_rejects_entries_naming_a_different_image() {
        // Tampered entry: urlbase A but the filename of image B (which
        // exists). The pipeline must re-download A rather than skip it —
        // otherwise A is never actually on disk under its own name.
        let dir = tempfile::tempdir().unwrap();
        let b = entry_with_file(dir.path(), "20260806", "B_ROW2");
        let mut tampered = entry_with_file(dir.path(), "20260807", "A_ROW1");
        fs::remove_file(&tampered.filename).unwrap();
        tampered.filename = b.filename.clone();
        let cat = Catalogue {
            images: vec![tampered, b.clone()],
        };

        assert_eq!(cat.existing_file(&urlbase("A_ROW1"), dir.path()), None);
        // The legitimate entry is unaffected.
        assert_eq!(
            cat.existing_file(&urlbase("B_ROW2"), dir.path()),
            Some(b.filename.clone())
        );
    }

    #[test]
    fn existing_file_rejects_entries_pointing_outside_the_download_dir() {
        // The same containment gate `prune` and the thumbnail backfill apply:
        // a hand-edited entry naming a file elsewhere must not pass for a
        // downloaded image (the pipeline re-downloads and the merge heals it).
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let entry = entry_with_file(&elsewhere, "20260807", "Foo_ROW1");
        let cat = Catalogue {
            images: vec![entry.clone()],
        };

        assert_eq!(cat.existing_file(&urlbase("Foo_ROW1"), dir.path()), None);
        // …and is found when asked about its own directory.
        assert_eq!(
            cat.existing_file(&urlbase("Foo_ROW1"), &elsewhere),
            Some(entry.filename)
        );
    }

    #[test]
    fn within_retention_is_the_cutoff_prune_applies() {
        // Shared with the thumbnail backfill, which uses it to skip entries
        // the imminent prune deletes — the two must not drift.
        let dir = tempfile::tempdir().unwrap();
        let entry = entry_with_file(dir.path(), "20260801", "Old_ROW1");
        let now = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();

        assert!(entry.within_retention(0, now), "0 days = keep forever");
        assert!(entry.within_retention(30, now));
        assert!(!entry.within_retention(8, now), "older than the cutoff");
        // Malformed dates are never deleted on a guess.
        let mut undated = entry.clone();
        undated.fullstartdate = "not a date".to_owned();
        assert!(undated.within_retention(1, now));
    }

    #[test]
    fn merge_heals_an_entry_pointing_at_a_different_images_file() {
        // Follow-up to the tampered `existing_file` case: the pipeline
        // re-downloaded image A (the tampered claim was rejected), and the
        // merge must adopt the fresh legitimate path even though the
        // tampered path still exists on disk.
        let dir = tempfile::tempdir().unwrap();
        let other = entry_with_file(dir.path(), "20260806", "B_ROW2");
        let mut tampered = entry_with_file(dir.path(), "20260807", "A_ROW1");
        let legit_path = tampered.filename.clone();
        fs::remove_file(&legit_path).unwrap();
        tampered.filename = other.filename.clone();
        let mut cat = Catalogue {
            images: vec![tampered.clone(), other.clone()],
        };

        fs::write(&legit_path, b"fresh jpeg").unwrap();
        let mut incoming = tampered.clone();
        incoming.filename = legit_path.clone();
        cat.merge(vec![incoming]);

        let healed = cat
            .images
            .iter()
            .find(|e| e.urlbase == urlbase("A_ROW1"))
            .unwrap();
        assert_eq!(healed.filename, legit_path);
        // The legitimate B entry keeps its own file.
        let b_entry = cat
            .images
            .iter()
            .find(|e| e.urlbase == urlbase("B_ROW2"))
            .unwrap();
        assert_eq!(b_entry.filename, other.filename);
    }

    #[test]
    fn merge_adopts_the_fresh_download_when_the_old_file_vanished() {
        let dir = tempfile::tempdir().unwrap();
        // A real (non-rebuilt) entry whose file vanished externally.
        let real = entry_with_file(dir.path(), "20260807", "Foo_ROW1");
        fs::remove_file(&real.filename).unwrap();
        let mut cat = Catalogue {
            images: vec![real.clone()],
        };

        // The pipeline re-downloaded the image (existing_file missed) —
        // the incoming entry carries the fresh file.
        let fresh_path = dir.path().join("20260807-Foo_ROW1_UHD_fresh.jpg");
        fs::write(&fresh_path, b"fresh jpeg").unwrap();
        let mut incoming = real.clone();
        incoming.filename = fresh_path.clone();
        cat.merge(vec![incoming]);

        // The entry now points at the fresh download instead of the
        // vanished path (which prune would drop, orphaning the download).
        assert_eq!(cat.images.len(), 1);
        assert_eq!(cat.images[0].filename, fresh_path);
        assert_eq!(cat.images[0].title, real.title); // metadata untouched
    }

    #[test]
    fn merge_adopts_newer_dates_when_bing_repeats_an_image() {
        // Bing re-runs an image under an unchanged urlbase on a later
        // day. The existing (real-metadata) entry must adopt the new
        // dates, or `newest()` would target yesterday's image and the
        // refresh schedule would derive from a stale date.
        let dir = tempfile::tempdir().unwrap();
        let old = entry_with_file(dir.path(), "20240101", "Foo_ROW1");
        let newer = entry_with_file(dir.path(), "20260807", "Bar_ROW2");
        let mut cat = Catalogue {
            images: vec![old.clone(), newer.clone()],
        };

        let mut incoming = old.clone();
        incoming.startdate = "20260808".to_owned();
        incoming.fullstartdate = "202608080700".to_owned();
        incoming.filename = dir.path().join("20260808-Foo_ROW1_UHD.jpg");
        cat.merge(vec![incoming]);

        let repeated = cat
            .images
            .iter()
            .find(|e| e.urlbase == urlbase("Foo_ROW1"))
            .unwrap();
        assert_eq!(repeated.startdate, "20260808");
        assert_eq!(repeated.fullstartdate, "202608080700");
        // The already-downloaded file (named with the old date) is kept.
        assert_eq!(repeated.filename, old.filename);
        assert_eq!(repeated.title, old.title);
        // The re-run is now the newest — auto-apply and scheduling
        // follow it.
        assert_eq!(cat.newest().unwrap().urlbase, urlbase("Foo_ROW1"));
    }

    #[test]
    fn merge_never_moves_dates_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let real = entry_with_file(dir.path(), "20260807", "Foo_ROW1");
        let mut cat = Catalogue {
            images: vec![real.clone()],
        };

        let mut incoming = real.clone();
        incoming.startdate = "20240101".to_owned();
        incoming.fullstartdate = "202401010700".to_owned();
        cat.merge(vec![incoming]);

        assert_eq!(cat.images, vec![real]);
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
                "copyright":"Nyhavn Canal, Copenhagen (© emicristea/Getty Images)","wp":true}]}"#,
        )
        .unwrap()
        .eligible[0]
            .image
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

        let deleted = cat.prune(dir.path(), 3, None, now());

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

        let deleted = cat.prune(dir.path(), 0, None, now());

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

        let deleted = cat.prune(dir.path(), 3, Some(&old_applied.filename), now());

        assert_eq!(deleted, vec![old_other.filename.clone()]);
        assert!(old_applied.filename.exists());
        assert_eq!(cat.images, vec![old_applied]);
    }

    #[test]
    fn prune_protecting_exempts_every_named_file_from_the_age_rule_only() {
        let dir = tempfile::tempdir().unwrap();
        let applied = entry_with_file(dir.path(), "20260701", "Applied_ROW1");
        let fallback = entry_with_file(dir.path(), "20260702", "Fallback_ROW2");
        let other = entry_with_file(dir.path(), "20260701", "Other_ROW3");
        let vanished = entry_with_file(dir.path(), "20260701", "Vanished_ROW4");
        std::fs::remove_file(&vanished.filename).unwrap();
        let mut cat = Catalogue {
            images: vec![
                applied.clone(),
                fallback.clone(),
                other.clone(),
                vanished.clone(),
            ],
        };

        let deleted = cat.prune_protecting(
            dir.path(),
            3,
            &[&applied.filename, &fallback.filename, &vanished.filename],
            now(),
        );

        assert_eq!(
            deleted,
            vec![other.filename.clone(), vanished.filename.clone()],
            "unprotected old file deleted; a protected path with no file is still dropped"
        );
        assert!(applied.filename.exists());
        assert!(fallback.filename.exists());
        assert_eq!(cat.images, vec![applied, fallback]);
    }

    #[test]
    fn prune_drops_entries_whose_file_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let gone = entry_with_file(dir.path(), "20260807", "Gone_ROW1");
        fs::remove_file(&gone.filename).unwrap();
        let there = entry_with_file(dir.path(), "20260807", "There_ROW2");
        let mut cat = Catalogue {
            images: vec![gone.clone(), there.clone()],
        };

        // Even with retention "forever", vanished entries are dropped —
        // and reported, so the caller can clean up their thumbnails.
        let removed = cat.prune(dir.path(), 0, None, now());

        assert_eq!(removed, vec![gone.filename]);
        assert_eq!(cat.images, vec![there]);
    }

    #[test]
    fn prune_keeps_the_entry_when_deleting_its_file_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("ro");
        fs::create_dir(&sub).unwrap();
        let old = entry_with_file(&sub, "20260701", "Old_ROW1");
        let mut cat = Catalogue {
            images: vec![old.clone()],
        };
        // Read-only parent dir → remove_file fails (for non-root).
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();
        // Verify the arrangement instead of trusting it: as root the mode is
        // ignored, and a test that quietly asserts nothing is worse than one
        // that fails.
        let blocked = fs::write(sub.join(".probe"), b"x").is_err();

        let removed = cat.prune(&sub, 3, None, now());

        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            blocked,
            "a read-only parent dir did not block writes — running as root? \
             the delete-failure path cannot be exercised here"
        );
        // Deletion failed as arranged: the entry must survive so the next
        // prune retries — dropping it would orphan the file.
        assert!(removed.is_empty());
        assert_eq!(cat.images, vec![old.clone()]);
        assert!(old.filename.exists());
    }

    #[test]
    fn prune_keeps_the_exact_cutoff_boundary() {
        let dir = tempfile::tempdir().unwrap();
        // now = 2026-08-07 12:00, retention 3 → cutoff 2026-08-04 12:00.
        // The comparison is strict `<`: exactly-at-cutoff is kept.
        let mut boundary = entry_with_file(dir.path(), "20260804", "Edge_ROW1");
        boundary.fullstartdate = "202608041200".to_owned();
        let mut cat = Catalogue {
            images: vec![boundary.clone()],
        };

        let removed = cat.prune(dir.path(), 3, None, now());

        assert!(removed.is_empty());
        assert_eq!(cat.images, vec![boundary.clone()]);
        assert!(boundary.filename.exists());
    }

    #[test]
    fn prune_keeps_malformed_dates() {
        let dir = tempfile::tempdir().unwrap();
        let mut odd = entry_with_file(dir.path(), "20260101", "Odd_ROW1");
        odd.fullstartdate = "not-a-date".to_owned();
        let mut cat = Catalogue {
            images: vec![odd.clone()],
        };

        cat.prune(dir.path(), 3, None, now());

        assert_eq!(cat.images, vec![odd]); // never delete on a guess
    }

    #[test]
    fn remove_ineligible_mirrors_prune_rule_for_rule() {
        // Only the named URL bases go; the applied file is exempt; an
        // already-vanished file still lets its entry go (the file-level
        // guarantee is met); a tampered entry pointing at a foreign file is
        // scrubbed with the file left untouched.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("BingWallpaper");
        fs::create_dir_all(&images).unwrap();
        let victim = dir.path().join("important.pdf");
        fs::write(&victim, b"precious").unwrap();
        let mut hostile = entry_with_file(&images, "20200101", "Evil_ROW1");
        fs::remove_file(&hostile.filename).unwrap();
        hostile.filename = victim.clone();
        let vanished = entry_with_file(&images, "20200102", "Vanished_ROW2");
        fs::remove_file(&vanished.filename).unwrap();
        let shown = entry_with_file(&images, "20200103", "Shown_ROW3");
        let restricted = entry_with_file(&images, "20200104", "Restricted_ROW4");
        let kept = entry_with_file(&images, "20260807", "Kept_ROW5");
        let mut cat = Catalogue {
            images: vec![
                hostile,
                vanished.clone(),
                shown.clone(),
                restricted.clone(),
                kept.clone(),
            ],
        };
        let ineligible = [
            urlbase("Evil_ROW1"),
            urlbase("Vanished_ROW2"),
            urlbase("Shown_ROW3"),
            urlbase("Restricted_ROW4"),
        ];

        let removed = cat.remove_ineligible(&ineligible, &images, Some(&shown.filename));

        assert!(victim.is_file(), "foreign file must never be deleted");
        assert!(shown.filename.is_file(), "the applied file is exempt");
        assert!(!restricted.filename.exists());
        assert!(kept.filename.is_file());
        assert_eq!(cat.images, vec![shown, kept]);
        assert_eq!(
            removed,
            vec![victim, vanished.filename, restricted.filename]
        );

        // An empty list is a no-op even against an unreadable folder.
        assert!(cat.remove_ineligible(&[], &images, None).is_empty());
    }

    #[test]
    fn prune_never_deletes_files_outside_the_images_dir() {
        // The tampered-catalogue scenario: an entry with an old
        // fullstartdate pointing at a user file elsewhere. Prune must not
        // honor it with deletion — the entry is scrubbed, the file stays.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("BingWallpaper");
        fs::create_dir_all(&images).unwrap();
        let victim = dir.path().join("important.pdf");
        fs::write(&victim, b"precious").unwrap();
        let mut hostile = entry_with_file(&images, "20200101", "Evil_ROW1");
        fs::remove_file(&hostile.filename).unwrap();
        hostile.filename = victim.clone();
        // A pattern-valid wallpaper filename *outside* the images dir is
        // just as foreign — dir containment alone must protect it.
        let outside = entry_with_file(dir.path(), "20200102", "Outside_ROW2");
        let kept = entry_with_file(&images, "20260807", "Kept_ROW3");
        let mut cat = Catalogue {
            images: vec![hostile, outside.clone(), kept.clone()],
        };

        let removed = cat.prune(&images, 3, None, now());

        assert!(victim.is_file(), "foreign file must never be deleted");
        assert!(outside.filename.is_file(), "outside-dir file must survive");
        assert_eq!(cat.images, vec![kept]); // hostile entries scrubbed
        // Reported so their cached thumbnails (namespaced by file name
        // under the thumbs dir) are cleaned up like any removal.
        assert_eq!(removed, vec![victim, outside.filename.clone()]);
    }

    #[test]
    fn prune_never_deletes_non_wallpaper_names_inside_the_images_dir() {
        // The download folder is shared with the user's own files: an
        // entry whose name fails the wallpaper pattern is not ours even
        // when it lives inside the images dir.
        let dir = tempfile::tempdir().unwrap();
        let vacation = dir.path().join("vacation.jpg");
        fs::write(&vacation, b"mine").unwrap();
        let mut hostile = entry_with_file(dir.path(), "20200101", "Evil_ROW1");
        fs::remove_file(&hostile.filename).unwrap();
        hostile.filename = vacation.clone();
        let mut cat = Catalogue {
            images: vec![hostile],
        };

        let removed = cat.prune(dir.path(), 3, None, now());

        assert!(
            vacation.is_file(),
            "non-wallpaper file must never be deleted"
        );
        assert!(cat.images.is_empty());
        assert_eq!(removed, vec![vacation]);
    }

    #[test]
    fn prune_never_deletes_a_different_images_file() {
        // Tampered catalogue: an old entry (urlbase Evil) pointing at
        // *another* image's perfectly valid wallpaper file inside the
        // images dir. The dir + pattern gate alone would pass it — the
        // urlbase-consistency gate must not: an entry may only delete
        // the file it legitimately names.
        let dir = tempfile::tempdir().unwrap();
        let victim = entry_with_file(dir.path(), "20260807", "Victim_ROW2");
        let mut hostile = entry_with_file(dir.path(), "20200101", "Evil_ROW1");
        fs::remove_file(&hostile.filename).unwrap();
        hostile.filename = victim.filename.clone();
        let mut cat = Catalogue {
            images: vec![hostile, victim.clone()],
        };

        let removed = cat.prune(dir.path(), 3, None, now());

        assert!(
            victim.filename.is_file(),
            "another image's file must never be deleted"
        );
        assert_eq!(cat.images, vec![victim.clone()]);
        // The scrubbed entry's path is still held by the surviving entry,
        // so it must NOT be reported — the caller would delete the
        // survivor's cached thumbnail.
        assert!(removed.is_empty());
    }

    #[test]
    fn tampered_catalogue_json_cannot_delete_arbitrary_files() {
        // End-to-end through the JSON boundary: a hand-edited catalogue
        // on disk round-trips through load + prune without the victim
        // file being touched, and the hostile entry does not survive.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("BingWallpaper");
        fs::create_dir_all(&images).unwrap();
        let victim = dir.path().join("Documents").join("important.pdf");
        fs::create_dir_all(victim.parent().unwrap()).unwrap();
        fs::write(&victim, b"precious").unwrap();
        let json = serde_json::json!({
            "images": [{
                "urlbase": "/th?id=OHR.Evil_ROW1",
                "startdate": "20200101",
                "fullstartdate": "202001010700",
                "title": "Evil",
                "copyright": "© Nobody",
                "copyrightlink": "https://example.com",
                "filename": victim,
            }]
        });
        let path = dir.path().join(CATALOGUE_FILENAME);
        fs::write(&path, json.to_string()).unwrap();

        let mut cat = Catalogue::load(&path).unwrap();
        let removed = cat.prune(&images, 3, None, now());

        assert!(victim.is_file(), "tampered entry must not delete the file");
        assert!(cat.images.is_empty());
        assert_eq!(removed, vec![victim]);
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
    fn random_other_never_returns_current_and_actually_varies() {
        let dir = tempfile::tempdir().unwrap();
        let a = entry_with_file(dir.path(), "20260805", "A_ROW1");
        let b = entry_with_file(dir.path(), "20260806", "B_ROW2");
        let c = entry_with_file(dir.path(), "20260807", "C_ROW3");
        let cat = Catalogue {
            images: vec![a.clone(), b.clone(), c.clone()],
        };

        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            let picked = cat.random_other(Some(&b.filename)).unwrap();
            assert_ne!(picked.filename, b.filename);
            seen.insert(picked.filename.clone());
        }
        // The property shuffle actually needs: successive picks differ. An
        // implementation always returning the first candidate satisfies
        // "never the current one" and would leave the wallpaper stuck.
        assert!(
            seen.len() > 1,
            "shuffle picked the same image every time: {seen:?}"
        );
        // With no current, any image qualifies — but it must pick one.
        assert!(cat.random_other(None).is_some());
    }

    #[test]
    fn random_other_survives_duplicate_filenames_without_panicking() {
        // Tampered-catalogue shape: two entries (different urlbases) both
        // pointing at the current file. `images.len() >= 2` passes but
        // every candidate is filtered out — must yield None, not a
        // modulo-by-zero panic.
        let dir = tempfile::tempdir().unwrap();
        let a = entry_with_file(dir.path(), "20260807", "A_ROW1");
        let mut b = entry_with_file(dir.path(), "20260806", "B_ROW2");
        b.filename = a.filename.clone();
        let cat = Catalogue {
            images: vec![b, a.clone()],
        };

        assert!(cat.random_other(Some(&a.filename)).is_none());
    }

    #[test]
    fn prune_reports_no_path_sharing_a_survivors_basename() {
        // Thumbnails are keyed by basename: a scrubbed foreign path that
        // merely *shares* a surviving entry's basename must not be
        // reported, or the caller would delete the survivor's thumbnail.
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("BingWallpaper");
        fs::create_dir_all(&images).unwrap();
        let kept = entry_with_file(&images, "20260807", "Kept_ROW1");
        let elsewhere = dir.path().join(kept.filename.file_name().unwrap());
        fs::write(&elsewhere, b"foreign copy").unwrap();
        let mut hostile = kept.clone();
        hostile.urlbase = urlbase("Evil_ROW2");
        hostile.filename = elsewhere.clone();
        let mut cat = Catalogue {
            images: vec![hostile, kept.clone()],
        };

        let removed = cat.prune(&images, 3, None, now());

        assert!(elsewhere.is_file(), "foreign file must never be deleted");
        assert_eq!(cat.images, vec![kept]);
        assert!(
            removed.is_empty(),
            "a path with a survivor's basename must not be reported"
        );
    }
}
