// Test-only helpers: a minimal loopback HTTP mock server so the network
// branches of `bing.rs`/`app.rs` run hermetically (nothing ever reaches
// the real Bing), plus an in-memory tiny-JPEG factory.

use std::io::{Read, Write};
use std::net::TcpListener;

/// Spawn a mock HTTP server on a random loopback port and return its base
/// URL (`http://127.0.0.1:<port>`). `routes` maps a request path (with
/// query string) to `(status, body)`. The server thread runs detached for
/// the rest of the test process; every response closes its connection so
/// the client's pool never holds a stale socket.
pub(crate) fn spawn_mock(routes: impl Fn(&str) -> (u16, Vec<u8>) + Send + 'static) -> String {
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
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        }
    });
    base
}

/// A real, decodable JPEG of the given size, in memory (for mock download
/// bodies that must pass both the magic-byte check and thumbnailing).
pub(crate) fn tiny_jpeg(width: u32, height: u32) -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    }));
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Jpeg)
        .expect("encode in-memory jpeg");
    buf.into_inner()
}
