//! Minimal HTTP echo server for benchmarking fetch() round-trips.
//! Returns `{"echo":true}` for every request.

use std::io::{Read, Write};
use std::net::TcpListener;

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(8888);

    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).unwrap();
    eprintln!("[echo-server] http://0.0.0.0:{port}");

    const BODY: &[u8] = b"{\"echo\":true}";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        BODY.len(),
        std::str::from_utf8(BODY).unwrap(),
    );
    let response_bytes = response.as_bytes().to_vec();

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let resp = response_bytes.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = match stream.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(_) => return,
                };
                // Naive: assume each read contains a complete request
                // For benchmarking this is fine
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    if stream.write_all(&resp).is_err() {
                        return;
                    }
                }
            }
        });
    }
}
