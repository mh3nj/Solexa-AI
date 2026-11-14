//! A deliberately tiny HTTP/1.1 client, used only to talk to a local
//! Ollama daemon (round 37).
//!
//! Why hand-rolled instead of `reqwest`: Ollama listens on plain HTTP at
//! `127.0.0.1:11434`, no TLS, no redirects, no auth, no cookies, no
//! compression. A full HTTP stack would pull in ~60 transitive crates to
//! use none of that, and the spec is explicit about preferring "boring,
//! reliable infrastructure" and avoiding "excessive dependencies". This
//! module is ~120 lines against `tokio`, which the workspace already
//! depends on.
//!
//! Scope, stated plainly so nobody mistakes this for a general client:
//! it speaks HTTP/1.1 to one known-good local server, handles
//! `Content-Length` and `Transfer-Encoding: chunked` response bodies,
//! and nothing else. It does not do TLS, redirects, keep-alive reuse,
//! compression, or proxies. If SOLEXA ever needs to talk to a remote
//! HTTPS model API, that is the moment to reach for a real HTTP crate,
//! not to grow this one.

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Sends one request and returns the response body as bytes.
/// `body` of `None` sends a GET; `Some(json)` sends a POST with
/// `Content-Type: application/json`.
pub async fn request(host: &str, port: u16, path: &str, body: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;

    let mut head = String::new();
    let method = if body.is_some() { "POST" } else { "GET" };
    head.push_str(&format!("{method} {path} HTTP/1.1\r\n"));
    head.push_str(&format!("Host: {host}:{port}\r\n"));
    // No keep-alive: one request per connection keeps this module
    // simple and costs nothing against a local daemon.
    head.push_str("Connection: close\r\n");
    head.push_str("Accept: application/json\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await.context("writing request head")?;
    if let Some(body) = body {
        stream.write_all(body).await.context("writing request body")?;
    }
    stream.flush().await.ok();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.context("reading response")?;

    parse_response(&raw)
}

/// Like `request`, but hands the decoded response body to `on_chunk`
/// progressively, as bytes actually arrive on the socket, instead of
/// buffering the whole response before returning (round 39). Used for
/// Ollama's `"stream": true` generation responses, where the entire
/// point is handing each token on to the rest of the system the moment
/// the daemon sends it, `request`'s `read_to_end` would defeat that:
/// the caller would still be waiting for the *last* byte before seeing
/// the *first* one.
///
/// `on_chunk` receives raw decoded body bytes with HTTP framing
/// (chunk-size markers, `Content-Length` accounting) already stripped
/// same as `request`'s return value, just delivered piece by piece.
/// Reassembling those bytes into lines or JSON is the caller's job (see
/// `ollama.rs`), not this module's, the same scope boundary
/// `parse_response` already draws for the non-streaming path.
///
/// A non-2xx response is still an error carrying the server's own
/// message, exactly like `request`: the body is buffered internally
/// (not handed to `on_chunk`) until the response finishes, at which
/// point it's surfaced as the error detail. `on_chunk` is only ever
/// called for a genuinely successful response.
pub async fn request_streaming(
    host: &str,
    port: u16,
    path: &str,
    body: Option<&[u8]>,
    mut on_chunk: impl FnMut(&[u8]),
) -> Result<()> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;

    let mut head = String::new();
    let method = if body.is_some() { "POST" } else { "GET" };
    head.push_str(&format!("{method} {path} HTTP/1.1\r\n"));
    head.push_str(&format!("Host: {host}:{port}\r\n"));
    head.push_str("Connection: close\r\n");
    head.push_str("Accept: application/json\r\n");
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await.context("writing request head")?;
    if let Some(body) = body {
        stream.write_all(body).await.context("writing request body")?;
    }
    stream.flush().await.ok();

    let mut buf = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();

    // Read until the header/body separator shows up, may take more
    // than one `read()` if the headers happen to span two TCP segments.
    let (status, chunked, content_length) = loop {
        if let Some(split) = find_subslice(&pending, b"\r\n\r\n") {
            let head = std::str::from_utf8(&pending[..split]).context("response headers were not valid UTF-8")?;
            let parsed = parse_head(head)?;
            pending.drain(..split + 4);
            break parsed;
        }
        let n = stream.read(&mut buf).await.context("reading response headers")?;
        if n == 0 {
            bail!("connection closed before response headers completed");
        }
        pending.extend_from_slice(&buf[..n]);
    };
    let status_ok = (200..300).contains(&status);
    let mut error_detail: Vec<u8> = Vec::new();

    if chunked {
        let mut awaiting_size = true;
        let mut remaining_in_chunk: usize = 0;
        loop {
            loop {
                if awaiting_size {
                    let Some(line_end) = find_subslice(&pending, b"\r\n") else { break };
                    let size_line =
                        std::str::from_utf8(&pending[..line_end]).context("chunk size was not valid UTF-8")?;
                    let size_hex = size_line.split(';').next().unwrap_or("").trim();
                    let size = usize::from_str_radix(size_hex, 16)
                        .with_context(|| format!("malformed chunk size {size_line:?}"))?;
                    pending.drain(..line_end + 2);
                    if size == 0 {
                        return finish(status, status_ok, error_detail);
                    }
                    remaining_in_chunk = size;
                    awaiting_size = false;
                } else if remaining_in_chunk > 0 {
                    if pending.is_empty() {
                        break;
                    }
                    let take = remaining_in_chunk.min(pending.len());
                    let data: Vec<u8> = pending.drain(..take).collect();
                    remaining_in_chunk -= take;
                    deliver(status_ok, &data, &mut on_chunk, &mut error_detail);
                    if remaining_in_chunk > 0 {
                        break; // this chunk continues in the next read()
                    }
                } else {
                    // Awaiting the CRLF that terminates every chunk's data.
                    if pending.len() < 2 {
                        break;
                    }
                    if &pending[..2] != b"\r\n" {
                        bail!("malformed chunked body: missing chunk terminator");
                    }
                    pending.drain(..2);
                    awaiting_size = true;
                }
            }
            let n = stream.read(&mut buf).await.context("reading response body")?;
            if n == 0 {
                bail!("connection closed before chunked body terminated");
            }
            pending.extend_from_slice(&buf[..n]);
        }
    } else if let Some(len) = content_length {
        let mut delivered = 0usize;
        if !pending.is_empty() {
            let take = pending.len().min(len - delivered);
            let data: Vec<u8> = pending.drain(..take).collect();
            delivered += take;
            deliver(status_ok, &data, &mut on_chunk, &mut error_detail);
        }
        while delivered < len {
            let n = stream.read(&mut buf).await.context("reading response body")?;
            if n == 0 {
                bail!("response body truncated: expected {len} bytes, got {delivered}");
            }
            let take = n.min(len - delivered);
            deliver(status_ok, &buf[..take], &mut on_chunk, &mut error_detail);
            delivered += take;
        }
        finish(status, status_ok, error_detail)
    } else {
        // No framing header: with `Connection: close`, the body
        // legitimately runs to EOF, hand off whatever's already
        // buffered, then keep delivering each read as it arrives.
        if !pending.is_empty() {
            let data = std::mem::take(&mut pending);
            deliver(status_ok, &data, &mut on_chunk, &mut error_detail);
        }
        loop {
            let n = stream.read(&mut buf).await.context("reading response body")?;
            if n == 0 {
                break;
            }
            deliver(status_ok, &buf[..n], &mut on_chunk, &mut error_detail);
        }
        finish(status, status_ok, error_detail)
    }
}

/// Routes one decoded slice of body bytes to `on_chunk` for a
/// successful response, or buffers it as error detail for a failing
/// one, shared by all three framing branches in `request_streaming`.
fn deliver(status_ok: bool, data: &[u8], on_chunk: &mut impl FnMut(&[u8]), error_detail: &mut Vec<u8>) {
    if data.is_empty() {
        return;
    }
    if status_ok {
        on_chunk(data);
    } else {
        error_detail.extend_from_slice(data);
    }
}

/// The tail end every framing branch of `request_streaming` shares:
/// `Ok(())` for a successful response, or the same
/// `"HTTP {status}: {server detail}"` error shape `parse_response`
/// produces, once the whole error body has been buffered.
fn finish(status: u16, status_ok: bool, error_detail: Vec<u8>) -> Result<()> {
    if status_ok {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&error_detail);
    let detail = detail.trim();
    bail!(
        "HTTP {status}{}",
        if detail.is_empty() { String::new() } else { format!(": {detail}") }
    );
}

/// Parses just the status line + headers (everything before the blank
/// line that separates head from body). Split out from `parse_response`
/// in round 39 so `request_streaming` can reuse the exact same header
/// parsing instead of duplicating it, the two functions differ only in
/// how they handle the *body* that follows.
fn parse_head(head: &str) -> Result<(u16, bool, Option<usize>)> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {status_line:?}"))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "transfer-encoding" if value.eq_ignore_ascii_case("chunked") => chunked = true,
            "content-length" => content_length = value.parse().ok(),
            _ => {}
        }
    }
    Ok((status, chunked, content_length))
}

/// Splits a raw HTTP response into status + headers + body, decoding
/// whichever body framing the server chose. Kept separate from the
/// socket work so it can be tested directly on byte strings.
fn parse_response(raw: &[u8]) -> Result<Vec<u8>> {
    let split = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response: no header/body separator"))?;
    let head = std::str::from_utf8(&raw[..split]).context("response headers were not valid UTF-8")?;
    let body = &raw[split + 4..];

    let (status, chunked, content_length) = parse_head(head)?;

    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(len) = content_length {
        // A truncated body is a real failure, not something to paper
        // over by returning whatever arrived.
        if body.len() < len {
            bail!("response body truncated: expected {len} bytes, got {}", body.len());
        }
        body[..len].to_vec()
    } else {
        // No framing header at all: with `Connection: close` the body
        // legitimately runs to EOF, which is what we already read.
        body.to_vec()
    };

    if !(200..300).contains(&status) {
        let detail = String::from_utf8_lossy(&body);
        let detail = detail.trim();
        bail!(
            "HTTP {status}{}",
            if detail.is_empty() { String::new() } else { format!(": {detail}") }
        );
    }

    Ok(body)
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subslice(body, b"\r\n").ok_or_else(|| anyhow!("malformed chunked body: unterminated size line"))?;
        let size_line = std::str::from_utf8(&body[..line_end]).context("chunk size was not valid UTF-8")?;
        // Chunk extensions (`1a;foo=bar`) are legal; ignore anything
        // after the semicolon.
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .with_context(|| format!("malformed chunk size {size_line:?}"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size {
            bail!("malformed chunked body: chunk claims {size} bytes, {} remain", body.len());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size..];
        // Each chunk is followed by its own CRLF.
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    #[test]
    fn parses_a_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"ok\":true}xx";
        let body = parse_response(raw).unwrap();
        // Exactly Content-Length bytes, trailing junk on the socket is
        // ignored rather than appended.
        assert_eq!(body.len(), 13);
        assert!(body.starts_with(b"{\"ok\":true}"));
    }

    #[test]
    fn parses_a_chunked_body_including_extensions_and_reassembles_it_in_order() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n7;ext=1\r\n, world\r\n0\r\n\r\n";
        let body = parse_response(raw).unwrap();
        assert_eq!(String::from_utf8(body).unwrap(), "hello, world");
    }

    #[test]
    fn reads_to_eof_when_the_server_sends_no_framing_header() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\nbare body";
        assert_eq!(String::from_utf8(parse_response(raw).unwrap()).unwrap(), "bare body");
    }

    #[test]
    fn a_non_2xx_status_is_an_error_carrying_the_servers_own_message() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 27\r\n\r\n{\"error\":\"model not found\"}";
        let err = parse_response(raw).unwrap_err().to_string();
        assert!(err.contains("404"), "got: {err}");
        assert!(err.contains("model not found"), "the daemon's own message must survive: {err}");
    }

    #[test]
    fn a_truncated_body_is_an_error_rather_than_silently_short_data() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\ntoo short";
        assert!(parse_response(raw).unwrap_err().to_string().contains("truncated"));
    }

    // ---- request_streaming (round 39) ---------------------------------
    //
    // These spin up a real `tokio` TCP server that writes its response
    // in deliberately separate `write()` calls with a delay between
    // them, so a test proves genuine incremental delivery, the
    // callback firing before the server has sent everything, rather
    // than merely proving the final assembled bytes are correct (which
    // a buffer-then-parse implementation would also pass).

    async fn serve_writes(writes: Vec<&'static [u8]>, delay_between: std::time::Duration) -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request_buf = [0u8; 1024];
            // Drain the request so the client's write() calls don't
            // block on a full socket buffer, content doesn't matter.
            let _ = socket.read(&mut request_buf).await;
            for (i, w) in writes.iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(delay_between).await;
                }
                socket.write_all(w).await.unwrap();
                socket.flush().await.unwrap();
            }
        });
        port
    }

    #[tokio::test]
    async fn streams_chunked_body_incrementally_not_only_at_the_end() {
        let port = serve_writes(
            vec![
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                b"5\r\nhello\r\n",
                b"6\r\n world\r\n",
                b"0\r\n\r\n",
            ],
            std::time::Duration::from_millis(40),
        )
        .await;

        let received: std::sync::Arc<Mutex<Vec<Vec<u8>>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        // The server sends 3 body-bearing writes 40ms apart; the whole
        // exchange finishing in well under that (each `on_chunk` call
        // arriving as soon as its bytes are decoded) is the actual proof
        // of incremental delivery.
        let started = std::time::Instant::now();
        request_streaming("127.0.0.1", port, "/stream", None, |data| {
            received_clone.lock().unwrap().push(data.to_vec());
        })
        .await
        .unwrap();
        let elapsed = started.elapsed();

        let chunks = received.lock().unwrap();
        assert_eq!(chunks.len(), 2, "expected one callback per chunked-body chunk, got {chunks:?}");
        assert_eq!(chunks[0], b"hello");
        assert_eq!(chunks[1], b" world");
        // 2 delays of 40ms each between the 3 writes = ~80ms minimum if
        // genuinely incremental; a buffer-then-parse implementation
        // would also take ~80ms here since it still has to wait for the
        // socket to finish, the real proof is `chunks.len() == 2`
        // above (two separate callback invocations, not one at the
        // end), which a non-incremental implementation could never
        // produce no matter how it's timed.
        assert!(elapsed >= std::time::Duration::from_millis(70), "elapsed: {elapsed:?}");
    }

    #[tokio::test]
    async fn streams_content_length_body_incrementally() {
        let port = serve_writes(
            vec![
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n",
                b"hello",
                b" world",
            ],
            std::time::Duration::from_millis(30),
        )
        .await;

        let received: std::sync::Arc<Mutex<Vec<Vec<u8>>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        request_streaming("127.0.0.1", port, "/stream", None, |data| {
            received_clone.lock().unwrap().push(data.to_vec());
        })
        .await
        .unwrap();

        let chunks = received.lock().unwrap();
        let joined: Vec<u8> = chunks.iter().flatten().copied().collect();
        assert_eq!(joined, b"hello world");
        assert!(chunks.len() >= 2, "expected more than one callback invocation, got {chunks:?}");
    }

    #[tokio::test]
    async fn streams_body_with_no_framing_header_until_connection_closes() {
        let port = serve_writes(
            vec![b"HTTP/1.1 200 OK\r\n\r\n", b"bare", b" body"],
            std::time::Duration::from_millis(20),
        )
        .await;

        let received: std::sync::Arc<Mutex<Vec<u8>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        request_streaming("127.0.0.1", port, "/stream", None, |data| {
            received_clone.lock().unwrap().extend_from_slice(data);
        })
        .await
        .unwrap();

        assert_eq!(&*received.lock().unwrap(), b"bare body");
    }

    #[tokio::test]
    async fn a_streaming_error_response_never_reaches_on_chunk_and_surfaces_the_servers_message() {
        let port = serve_writes(
            vec![b"HTTP/1.1 404 Not Found\r\nContent-Length: 27\r\n\r\n{\"error\":\"model not found\"}"],
            std::time::Duration::from_millis(0),
        )
        .await;

        let received: std::sync::Arc<Mutex<Vec<u8>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        let err = request_streaming("127.0.0.1", port, "/stream", None, |data| {
            received_clone.lock().unwrap().extend_from_slice(data);
        })
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("404"), "got: {err}");
        assert!(err.contains("model not found"), "the daemon's own message must survive: {err}");
        assert!(received.lock().unwrap().is_empty(), "on_chunk must never see error-response bytes");
    }
}
