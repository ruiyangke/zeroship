use crate::config::Config;

/// Build an HTTP/1.1 request as bytes.
pub fn build_request(config: &Config) -> Vec<u8> {
    let mut req = format!("{} {} HTTP/1.1\r\n", config.method, config.path);
    req.push_str(&format!("Host: {}:{}\r\n", config.host, config.port));
    req.push_str("Connection: keep-alive\r\n");

    for (name, value) in &config.headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }

    if let Some(body) = &config.body {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        req.push_str("\r\n");
        req.push_str(body);
    } else {
        req.push_str("\r\n");
    }

    req.into_bytes()
}

/// Parse result from httparse.
pub struct ParsedResponse {
    pub status: u16,
    pub header_len: usize,
    pub content_length: usize,
    pub keep_alive: bool,
    pub chunked: bool,
}

/// Parse HTTP response headers. Returns None if incomplete.
pub fn parse_response(buf: &[u8]) -> Option<ParsedResponse> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut resp = httparse::Response::new(&mut headers);

    match resp.parse(buf) {
        Ok(httparse::Status::Complete(header_len)) => {
            let status = resp.code.unwrap_or(0);
            let mut content_length = 0usize;
            let mut keep_alive = true;
            let mut chunked = false;

            for h in resp.headers.iter() {
                let name = h.name.to_lowercase();
                let val = std::str::from_utf8(h.value).unwrap_or("");
                match name.as_str() {
                    "content-length" => content_length = val.parse().unwrap_or(0),
                    "connection" => keep_alive = !val.eq_ignore_ascii_case("close"),
                    "transfer-encoding" => chunked = val.eq_ignore_ascii_case("chunked"),
                    _ => {}
                }
            }

            Some(ParsedResponse { status, header_len, content_length, keep_alive, chunked })
        }
        _ => None,
    }
}

/// Find the end of a chunked body. Returns total body length if complete.
pub fn find_chunked_end(body: &[u8]) -> Option<usize> {
    // Look for the terminating 0\r\n\r\n
    if body.len() >= 5 {
        for i in 0..body.len() - 4 {
            if &body[i..i + 5] == b"0\r\n\r\n" {
                return Some(i + 5);
            }
        }
    }
    None
}
