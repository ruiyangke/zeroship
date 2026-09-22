//! A control plane a worker case can address.
//!
//! It ANSWERS, which is what separates it from
//! `sync::tests::bindings::recording_control`: that one drops every connection
//! because the only thing it measures is the address a call was sent to, and
//! these cases measure what the worker did with the answer.
//!
//! Routes are exact paths, built by the caller from
//! `zeroship_core::service_identity::endpoints` with the app id filled in, so
//! a case pins the route control declares rather than a path spelled here. A
//! request for anything else is answered `404`, which is what control serves
//! for an app it holds nothing for.

use std::io::{Read, Write};
use std::net::TcpListener;

/// A control plane serving `routes`, stopping after `expected` requests.
///
/// The join handle yields the paths it was asked for, in order: a case reads
/// it to assert not only that a call was made but WHICH, and an empty vector
/// is the assertion that no call was made at all.
pub struct ControlPlane {
    pub base_url: String,
    served: std::thread::JoinHandle<Vec<String>>,
}

impl ControlPlane {
    /// Start one. `expected` bounds how many requests it answers before the
    /// listener closes, so a case that expects none closes immediately and a
    /// later call is refused rather than silently satisfied.
    pub fn serving(expected: usize, routes: Vec<(String, String)>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a control plane binds");
        let base_url = format!("http://{}", listener.local_addr().expect("its address"));
        listener
            .set_nonblocking(true)
            .expect("poll for connections so a call that never arrives ends the wait");
        let served = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut paths = Vec::new();
            while paths.len() < expected && std::time::Instant::now() < deadline {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept a control request: {error}"),
                };
                stream.set_nonblocking(false).expect("read the request head");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .expect("bound that read");
                let mut head = Vec::new();
                let mut chunk = [0_u8; 256];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split(' ')
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let response = match routes.iter().find(|(route, _)| route == &path) {
                    Some((_, body)) => http_response("200 OK", body),
                    None => http_response("404 Not Found", r#"{"error":"not found"}"#),
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                paths.push(path);
            }
            paths
        });
        Self { base_url, served }
    }

    /// The paths this control plane was asked for, in order.
    pub fn served(self) -> Vec<String> {
        self.served.join().expect("the control plane thread")
    }
}

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
}
