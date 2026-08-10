// Test-only helpers: a minimal loopback HTTP mock server so the network
// branches of `bing.rs`/`app.rs` run hermetically (nothing ever reaches
// the real Bing), an in-memory tiny-JPEG factory, the stock-palette
// accessors, the cosmic-config filesystem walkers (key-file lookup and
// read-only failure injection) shared by the accent/app/config test modules,
// and the tempdir-rooted cosmic-bg *state* builders shared by the lock-poke
// tests in `wallpaper.rs` and `app.rs`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use cosmic::cosmic_theme::{CosmicPaletteInner, DARK_PALETTE, LIGHT_PALETTE};

/// How the mock frames a response body.
#[derive(Clone, Copy)]
pub enum Framing {
    /// Accurate `Content-Length` — the honest server.
    Sized,
    /// No `Content-Length` at all: body, then EOF ends the message
    /// (RFC 9112 §6.3, legal for responses). This is what a `gzip`-encoded
    /// reply looks like to reqwest — the decoder's size is unknown, so
    /// `Response::content_length()` is `None` and only a streamed budget can
    /// bound the read.
    ///
    /// A *lying* `Content-Length` needs no mode of its own: hyper frames the
    /// message by the advertised length, so a short header simply truncates
    /// the body — it can never grow past the budget.
    UntilEof,
}

/// Spawn a mock HTTP server on a random loopback port and return its base
/// URL (`http://127.0.0.1:<port>`). `routes` maps a request path (with
/// query string) to `(status, body)`. The server thread runs detached for
/// the rest of the test process; every response closes its connection so
/// the client's pool never holds a stale socket.
pub fn spawn_mock(routes: impl Fn(&str) -> (u16, Vec<u8>) + Send + 'static) -> String {
    spawn_mock_framed(routes, Framing::Sized)
}

/// [`spawn_mock`] with the response framing chosen by the caller.
pub fn spawn_mock_framed(
    routes: impl Fn(&str) -> (u16, Vec<u8>) + Send + 'static,
    framing: Framing,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock server");
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read the request head (GET only — no bodies to consume).
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let head = String::from_utf8_lossy(&buf);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, body) = routes(&path);
            let head = match framing {
                Framing::Sized => format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                Framing::UntilEof => format!("HTTP/1.1 {status} Mock\r\nConnection: close\r\n\r\n"),
            };
            let _ = stream.write_all(head.as_bytes());
            // A client that gives up mid-body (an exceeded size budget) drops
            // its end, so the write failing here is expected — not a panic.
            let _ = stream.write_all(&body);
        }
    });
    base
}

/// The stock COSMIC light palette (what an untouched light builder carries).
pub fn light_palette() -> &'static CosmicPaletteInner {
    (*LIGHT_PALETTE).as_ref()
}

/// The stock COSMIC dark palette (what an untouched dark builder carries).
pub fn dark_palette() -> &'static CosmicPaletteInner {
    (*DARK_PALETTE).as_ref()
}

/// Locate the per-key RON file cosmic-config wrote for `key` somewhere under
/// `root` (tests neither know nor care about the version subdirectory).
pub fn find_key_file(root: &Path, key: &str) -> PathBuf {
    fn walk(dir: &Path, key: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = walk(&path, key) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| name == key) {
                return Some(path);
            }
        }
        None
    }
    walk(root, key).expect("key file written by cosmic-config")
}

/// Make every directory under `roots` read-only so writes into them fail —
/// the shared failure injection for "the config write did not land" branches.
/// Returns all affected directories for [`restore_dir_permissions`] (TempDir
/// cleanup needs them writable again).
pub fn read_only_trees(roots: &[PathBuf]) -> Vec<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    fn dirs_under(dir: &Path, out: &mut Vec<PathBuf>) {
        out.push(dir.to_path_buf());
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs_under(&path, out);
            }
        }
    }
    let mut dirs = Vec::new();
    for root in roots {
        dirs_under(root, &mut dirs);
    }
    for dir in &dirs {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    dirs
}

/// Undo [`read_only_trees`].
pub fn restore_dir_permissions(dirs: &[PathBuf]) {
    use std::os::unix::fs::PermissionsExt as _;
    for dir in dirs {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A tempdir-rooted stand-in for [`crate::wallpaper::poke_state_handle`]'s
/// production handle (which always roots in the real user state dir): same
/// identity (cosmic-bg's `NAME` + `State::version()`) so the raw-key layout
/// under `root` matches production shape; the custom path keeps it hermetic.
pub fn bg_state_config(root: &Path) -> cosmic::cosmic_config::Config {
    cosmic::cosmic_config::Config::with_custom_path(
        cosmic_bg_config::NAME,
        cosmic_bg_config::state::State::version(),
        root.to_path_buf(),
    )
    .expect("create tempdir-rooted cosmic-bg state config")
}

/// Where `with_custom_path` puts the lock poke's key file:
/// `<root>/cosmic/<name>/v<version>/wallpapers`.
///
/// The `"wallpapers"` literal here — and in the raw `get`/`set` calls of the
/// tests using these helpers — is a **deliberate pin** of cosmic-bg's
/// on-disk key name. Production goes through `wallpaper::WALLPAPERS_KEY`;
/// tests spell the string out so a drift in that const breaks them instead
/// of silently retargeting them.
pub fn bg_wallpapers_key_file(root: &Path) -> PathBuf {
    root.join("cosmic")
        .join(cosmic_bg_config::NAME)
        .join(format!("v{}", cosmic_bg_config::state::State::version()))
        .join("wallpapers")
}

/// The key file's inode — write assertions go by **inode** (cosmic-config's
/// `set` commits an `AtomicFile`, i.e. temp+rename, so every real write is a
/// new inode), never by mtime (the flakiness class `thumbs.rs` abandoned:
/// timestamp ties cannot distinguish "unchanged" from "changed").
pub fn bg_wallpapers_inode(root: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(bg_wallpapers_key_file(root))
        .expect("stat wallpapers key")
        .ino()
}

/// A `Source::Path` in the canonical download-dir shape, distinguished by
/// `name`.
pub fn bg_path_source(name: &str) -> cosmic_bg_config::Source {
    cosmic_bg_config::Source::Path(PathBuf::from(format!(
        "/home/u/Pictures/BingWallpaper/{name}.jpg"
    )))
}

/// A real, decodable JPEG of the given size, in memory (for mock download
/// bodies that must pass both the magic-byte check and thumbnailing).
pub fn tiny_jpeg(width: u32, height: u32) -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    }));
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Jpeg)
        .expect("encode in-memory jpeg");
    buf.into_inner()
}

/// Every runtime action an update's returned `Task` would emit, classified
/// by `pick` (a `None` is dropped), **in order** — the drain skeleton shared
/// by [`surface::emitted`] and `app::tests`' poke-rung collector. A `Task`
/// returned from `update()` is never polled in unit tests, so this is the
/// only way to observe what it would actually emit.
pub async fn drained_task_outputs<M, T>(
    task: cosmic::app::Task<M>,
    mut pick: impl FnMut(cosmic::iced::runtime::Action<cosmic::Action<M>>) -> Option<T>,
) -> Vec<T>
where
    M: Send + 'static,
{
    use cosmic::iced::futures::StreamExt as _;

    let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) else {
        return Vec::new();
    };
    let mut picked = Vec::new();
    while let Some(action) = stream.next().await {
        if let Some(item) = pick(action) {
            picked.push(item);
        }
    }
    picked
}

/// The popup-ledger side of the test surface: builders for the
/// `cosmic::surface::Action`s `app::Window::on_tooltip_surface` /
/// `on_dropdown_surface` route, plus a collector for the actions a returned
/// task actually emits.
///
/// Shared because `app::tests` and `view::tests` both drive the ledger through
/// `Window::update`, and because the ledger's whole job is *emission order* —
/// asserting the `Window` flags alone would keep passing with every destroy
/// deleted.
pub mod surface {
    use std::any::Any;
    use std::sync::Arc;

    use crate::app::Message;

    /// A `popup_dropdown` create. The settings payloads are opaque
    /// (`Arc<Box<dyn Any + ..>>`) and never executed here — the ledger only
    /// inspects the variant.
    pub fn dropdown_create() -> cosmic::surface::Action {
        cosmic::surface::Action::Popup(opaque(), opaque(), None)
    }

    /// The `AppPopup` shape of the same create. `popup_dropdown` mints
    /// `Action::Popup` at the pinned rev, so this arm is defensive — but the
    /// two are distinct variants, and a create falling through the catch-all
    /// would map a menu with the ledger believing nothing is open.
    pub fn app_dropdown_create() -> cosmic::surface::Action {
        cosmic::surface::Action::AppPopup(opaque(), opaque(), None)
    }

    /// A menu closing. Its id is minted inside the widget, so it is neither
    /// our popup's nor the tooltip's.
    pub fn dropdown_destroy() -> cosmic::surface::Action {
        cosmic::surface::Action::DestroyPopup(cosmic::iced::window::Id::unique())
    }

    /// The arm signal the tooltip widget actually publishes: with a delay set
    /// it emits `Action::Task`, and the create the future resolves to never
    /// comes back through `Message`.
    pub fn tooltip_arm() -> cosmic::surface::Action {
        cosmic::surface::Action::Task(Arc::new(cosmic::iced::Task::none))
    }

    /// The tooltip's `on_close`, naming the one shared tooltip surface.
    pub fn tooltip_destroy() -> cosmic::surface::Action {
        cosmic::surface::Action::DestroyPopup(crate::tooltip::window_id())
    }

    fn opaque() -> Arc<Box<dyn Any + Send + Sync>> {
        Arc::new(Box::new(()) as Box<dyn Any + Send + Sync>)
    }

    /// One surface action a task emitted, classified for assertions (the
    /// payloads are opaque and the ids are `unique()`, so the variant plus
    /// "is it the tooltip surface?" is everything that can be checked).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Emitted {
        /// A destroy naming [`crate::tooltip::window_id`].
        DestroyTooltip,
        /// A destroy naming anything else (our popup, a menu).
        DestroyOther,
        /// A popup create, in either variant.
        Create,
        /// The tooltip widget's delayed-create task.
        Arm,
        /// Anything else.
        Other,
    }

    impl Emitted {
        fn of(action: &cosmic::surface::Action) -> Self {
            match action {
                cosmic::surface::Action::DestroyPopup(id) if *id == crate::tooltip::window_id() => {
                    Self::DestroyTooltip
                }
                cosmic::surface::Action::DestroyPopup(_) => Self::DestroyOther,
                cosmic::surface::Action::Popup(..) | cosmic::surface::Action::AppPopup(..) => {
                    Self::Create
                }
                cosmic::surface::Action::Task(_) => Self::Arm,
                _ => Self::Other,
            }
        }
    }

    /// Every surface action `task` emits, **in order**.
    ///
    /// This is the only way to see what the ledger decided: the flags it sets
    /// are separate statements from the `surface_task(..)` calls, so a test
    /// that asserts flags alone passes with the emissions deleted.
    pub async fn emitted(task: cosmic::app::Task<Message>) -> Vec<Emitted> {
        super::drained_task_outputs(task, |action| match action {
            cosmic::iced::runtime::Action::Output(cosmic::Action::Cosmic(
                cosmic::app::Action::Surface(action),
            )) => Some(Emitted::of(&action)),
            _ => None,
        })
        .await
    }
}
