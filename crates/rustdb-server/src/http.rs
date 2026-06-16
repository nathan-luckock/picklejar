//! A minimal, hand-written HTTP/1.1 layer: enough to read a request and write
//! a response for the studio API. One request per connection, no keep-alive.

use std::io::{self, BufRead, BufReader, Read, Write};

/// A parsed HTTP request: the method, the path, and the body.
#[derive(Debug)]
pub struct Request {
    /// The HTTP method (`GET`, `POST`, `OPTIONS`, ...).
    pub method: String,
    /// The request path (query string stripped).
    pub path: String,
    /// The request body.
    pub body: String,
}

/// Read one request from `source`, or `None` if it closed before a request
/// line arrived.
///
/// # Errors
///
/// Returns an I/O error if the source cannot be read, or if the request line
/// is malformed.
pub fn read_request<R: Read>(source: R) -> io::Result<Option<Request>> {
    let mut reader = BufReader::new(source);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| bad("missing method"))?
        .to_string();
    let target = parts.next().ok_or_else(|| bad("missing path"))?;
    let path = target.split('?').next().unwrap_or(target).to_string();

    // Headers, up to the blank line. We only need Content-Length.
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    Ok(Some(Request { method, path, body }))
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Write a response with the given status, content type, and body, including
/// permissive CORS headers so a browser studio can call the API.
///
/// # Errors
///
/// Returns an I/O error if the sink cannot be written.
pub fn write_response<W: Write>(
    sink: &mut W,
    status: u16,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    let reason = match status {
        204 => "No Content",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    sink.write_all(response.as_bytes())?;
    sink.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_request_line_headers_and_body() {
        let raw = b"POST /api/query?foo=bar HTTP/1.1\r\nContent-Length: 9\r\nX: y\r\n\r\nSELECT 1;";
        let req = read_request(Cursor::new(&raw[..])).unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/query"); // query string stripped
        assert_eq!(req.body, "SELECT 1;");
    }

    #[test]
    fn empty_input_is_none() {
        assert!(read_request(Cursor::new(b"".as_slice())).unwrap().is_none());
    }

    #[test]
    fn response_has_status_cors_and_body() {
        let mut sink = Vec::new();
        write_response(&mut sink, 200, "application/json", r#"{"ok":true}"#).unwrap();
        let s = String::from_utf8(sink).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "{s}");
        assert!(s.contains("Access-Control-Allow-Origin: *"), "{s}");
        assert!(s.contains("Content-Length: 11"), "{s}");
        assert!(s.ends_with(r#"{"ok":true}"#), "{s}");
    }
}
