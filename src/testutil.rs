// Test-only helpers: a minimal loopback HTTP mock server so the network
// branches of `bing.rs`/`app.rs` run hermetically (nothing ever reaches
// the real Bing), an in-memory tiny-JPEG factory, the stock-palette
// accessors, and the cosmic-config filesystem walkers (key-file lookup and
// read-only failure injection) shared by the accent/app/config test modules.

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
