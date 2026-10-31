//! Serves the sign-in page from Connect's own address until the stack takes that
//! port over. Same origin means the same session key and the same session, so
//! Connect comes up already signed in. It reads the app's embedded files and
//! answers GET: it has one job and a few seconds to do it in.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tauri::AppHandle;

pub const SIGN_IN_PAGE: &str = "sign-in.html";

pub struct SignInServer {
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SignInServer {
    pub fn start(app: AppHandle, port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => answer(&app, stream),
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Ok(Self {
            stopping,
            thread: Some(thread),
        })
    }

    /// Returns once the port is free.
    pub fn stop(mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The path a request line asks for, without its query.
fn requested_path(request: &str) -> Option<&str> {
    let mut parts = request.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?.split(['?', '#']).next()?;
    (!target.contains("..")).then(|| target.trim_start_matches('/'))
}

fn answer(app: &AppHandle, mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buffer = [0u8; 2048];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let asset = request
        .lines()
        .next()
        .and_then(requested_path)
        .and_then(|path| app.asset_resolver().get(path.to_string()));
    let response = match asset {
        Some(asset) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                asset.mime_type,
                asset.bytes.len()
            );
            [head.into_bytes(), asset.bytes].concat()
        }
        None => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    let _ = stream.write_all(&response);
}

#[cfg(test)]
mod tests {
    use super::requested_path;

    #[test]
    fn serves_a_path_without_its_query() {
        assert_eq!(
            requested_path("GET /sign-in.html HTTP/1.1"),
            Some("sign-in.html")
        );
        assert_eq!(
            requested_path("GET /bg-0.svg?v=1 HTTP/1.1"),
            Some("bg-0.svg")
        );
    }

    #[test]
    fn answers_get_and_nothing_else() {
        assert_eq!(requested_path("POST /sign-in.html HTTP/1.1"), None);
        assert_eq!(requested_path(""), None);
    }

    #[test]
    fn refuses_to_climb_out() {
        assert_eq!(requested_path("GET /../secret HTTP/1.1"), None);
    }
}
