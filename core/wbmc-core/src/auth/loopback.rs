// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One-shot loopback HTTP listener that receives the OAuth redirect (RFC 8252 §7.3).

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crate::{CoreError, Result};

/// Upper bound on the bytes read from one connection (request line plus headers).
pub const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long one connection may take to deliver its request.
const PER_CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A listener bound to `127.0.0.1` on an OS-chosen port.
#[derive(Debug)]
pub struct Loopback {
    listener: std::net::TcpListener,
    port: u16,
}

impl Loopback {
    pub fn bind() -> Result<Self> {
        let listener =
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(network_error)?;
        listener.set_nonblocking(true).map_err(network_error)?;
        let port = listener.local_addr().map_err(network_error)?.port();
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI registered for public clients: `http://localhost:{port}`.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Waits for the redirect on `/`, checks `state`, and returns the authorization code.
    ///
    /// Requests for other paths (e.g. `/favicon.ico`), non-GET methods, and malformed or
    /// oversized requests get an error status and are otherwise ignored until `timeout`.
    pub async fn wait_for_code(self, expected_state: &str, timeout: Duration) -> Result<String> {
        let listener = TcpListener::from_std(self.listener).map_err(network_error)?;
        let deadline = Instant::now() + timeout;
        loop {
            let stream = match tokio::time::timeout_at(deadline, listener.accept()).await {
                Err(_) => {
                    return Err(CoreError::Network(
                        "timed out waiting for the browser to finish sign-in".into(),
                    ));
                }
                Ok(Err(e)) => return Err(network_error(e)),
                Ok(Ok((stream, _))) => stream,
            };
            let read_deadline = deadline.min(Instant::now() + PER_CONNECTION_READ_TIMEOUT);
            if let Some(result) = handle_connection(stream, expected_state, read_deadline).await {
                return result;
            }
        }
    }
}

fn network_error(e: std::io::Error) -> CoreError {
    CoreError::Network(format!("sign-in listener: {e}"))
}

const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>WBMC</title></head>\
<body><h1>Signed in</h1><p>You can close this window and return to WBMC.</p></body></html>";

const FAILURE_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>WBMC</title></head>\
<body><h1>Sign-in did not complete</h1><p>You can close this window and try again in WBMC.</p></body></html>";

const NOT_FOUND_PAGE: &str = "Not found";

/// Handles one connection. `None` means "not the callback; keep waiting".
async fn handle_connection(
    mut stream: TcpStream,
    expected_state: &str,
    read_deadline: Instant,
) -> Option<Result<String>> {
    let request = match read_request(&mut stream, read_deadline).await {
        ReadOutcome::Complete(bytes) => bytes,
        ReadOutcome::TooLarge => {
            respond(
                &mut stream,
                "431 Request Header Fields Too Large",
                NOT_FOUND_PAGE,
            )
            .await;
            return None;
        }
        ReadOutcome::Incomplete => {
            respond(&mut stream, "400 Bad Request", FAILURE_PAGE).await;
            return None;
        }
        ReadOutcome::Empty => return None,
    };

    // Only the request line matters; headers and body are ignored.
    let line_end = request
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(request.len());
    let Ok(line) = std::str::from_utf8(&request[..line_end]) else {
        respond(&mut stream, "400 Bad Request", FAILURE_PAGE).await;
        return None;
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(_version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        respond(&mut stream, "400 Bad Request", FAILURE_PAGE).await;
        return None;
    };
    if method != "GET" {
        respond(&mut stream, "405 Method Not Allowed", NOT_FOUND_PAGE).await;
        return None;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/" {
        respond(&mut stream, "404 Not Found", NOT_FOUND_PAGE).await;
        return None;
    }

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let slot = match key.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value.into_owned());
        }
    }

    // State is checked first so an unsolicited `?error=` without our state cannot end the wait
    // with a misleading provider error; it still fails closed as a mismatch.
    let result = if state.as_deref() != Some(expected_state) {
        Err(CoreError::InvalidInput(
            "sign-in response did not match this request (state mismatch); please try again".into(),
        ))
    } else if let Some(error) = error {
        // error_description is deliberately dropped: keep messages short and log-safe.
        Err(CoreError::InvalidInput(format!(
            "sign-in was not completed: {}",
            sanitize_error_code(&error)
        )))
    } else {
        match code {
            Some(code) if !code.is_empty() => Ok(code),
            _ => Err(CoreError::InvalidInput(
                "sign-in response had no authorization code".into(),
            )),
        }
    };
    match &result {
        Ok(_) => respond(&mut stream, "200 OK", SUCCESS_PAGE).await,
        Err(_) => respond(&mut stream, "400 Bad Request", FAILURE_PAGE).await,
    }
    Some(result)
}

enum ReadOutcome {
    Complete(Vec<u8>),
    TooLarge,
    Incomplete,
    Empty,
}

/// Reads up to the end of the headers, never more than `MAX_REQUEST_BYTES`.
async fn read_request(stream: &mut TcpStream, deadline: Instant) -> ReadOutcome {
    let mut buf = vec![0u8; MAX_REQUEST_BYTES];
    let mut len = 0;
    loop {
        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
            buf.truncate(len);
            return ReadOutcome::Complete(buf);
        }
        if len == MAX_REQUEST_BYTES {
            return ReadOutcome::TooLarge;
        }
        match tokio::time::timeout_at(deadline, stream.read(&mut buf[len..])).await {
            Ok(Ok(0)) if len == 0 => return ReadOutcome::Empty,
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return ReadOutcome::Incomplete,
            Ok(Ok(n)) => len += n,
        }
    }
}

/// Writes a static response; failures are ignored (the browser may already be gone).
async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let content_type = if body.starts_with("<!doctype") {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
Cache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\n\
Referrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let write = async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), write).await;
}

/// OAuth error codes are short ASCII identifiers; anything else is dropped.
fn sanitize_error_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::CoreError;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Sends raw bytes to the loopback port and returns whatever response arrives.
    pub(crate) async fn send_raw(port: u16, request: &[u8]) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        // The server may close early (oversized requests), so write errors are expected there.
        let _ = stream.write_all(request).await;
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response).await;
        String::from_utf8_lossy(&response).into_owned()
    }

    pub(crate) async fn send_get(port: u16, target: &str) -> String {
        let request = format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n");
        send_raw(port, request.as_bytes()).await
    }

    fn spawn_wait(
        expected_state: &'static str,
        timeout: Duration,
    ) -> (u16, tokio::task::JoinHandle<Result<String>>) {
        let loopback = Loopback::bind().unwrap();
        let port = loopback.port();
        let handle =
            tokio::spawn(async move { loopback.wait_for_code(expected_state, timeout).await });
        (port, handle)
    }

    #[test]
    fn redirect_uri_uses_localhost_and_port() {
        let loopback = Loopback::bind().unwrap();
        assert_ne!(loopback.port(), 0);
        assert_eq!(
            loopback.redirect_uri(),
            format!("http://localhost:{}", loopback.port())
        );
    }

    #[tokio::test]
    async fn loopback_accepts_matching_state() {
        let (port, handle) = spawn_wait("s1", Duration::from_secs(10));
        let response = send_get(port, "/?code=the-code&state=s1").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("You can close this window"));
        assert_eq!(handle.await.unwrap().unwrap(), "the-code");
    }

    #[tokio::test]
    async fn loopback_rejects_wrong_state() {
        let (port, handle) = spawn_wait("expected", Duration::from_secs(10));
        let response = send_get(port, "/?code=c&state=forged").await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn loopback_rejects_missing_state() {
        let (port, handle) = spawn_wait("expected", Duration::from_secs(10));
        send_get(port, "/?code=c").await;
        assert!(matches!(
            handle.await.unwrap().unwrap_err(),
            CoreError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    async fn loopback_ignores_favicon_then_accepts_callback() {
        let (port, handle) = spawn_wait("s1", Duration::from_secs(10));
        let favicon = send_get(port, "/favicon.ico").await;
        assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
        send_get(port, "/?code=good&state=s1").await;
        assert_eq!(handle.await.unwrap().unwrap(), "good");
    }

    #[tokio::test]
    async fn loopback_rejects_oversized_request() {
        let (port, handle) = spawn_wait("s1", Duration::from_secs(10));
        let mut big =
            format!("GET /?code=oversized&state=s1 HTTP/1.1\r\nHost: localhost:{port}\r\n");
        big.push_str(&format!("X-Pad: {}\r\n\r\n", "a".repeat(MAX_REQUEST_BYTES)));
        let response = send_raw(port, big.as_bytes()).await;
        assert!(
            response.is_empty() || response.starts_with("HTTP/1.1 431"),
            "{response}"
        );
        // The oversized request is discarded; the listener keeps waiting for a valid one.
        send_get(port, "/?code=good&state=s1").await;
        assert_eq!(handle.await.unwrap().unwrap(), "good");
    }

    #[tokio::test]
    async fn loopback_maps_provider_error_without_reflecting_input() {
        let (port, handle) = spawn_wait("s1", Duration::from_secs(10));
        let response = send_get(
            port,
            "/?error=access_denied&error_description=%3Cscript%3Ealert(1)%3C%2Fscript%3E&state=s1",
        )
        .await;
        assert!(!response.contains("script"), "{response}");
        assert!(!response.contains("access_denied"), "{response}");
        let err = handle.await.unwrap().unwrap_err();
        match err {
            CoreError::InvalidInput(message) => {
                assert!(message.contains("access_denied"), "{message}");
                assert!(!message.contains("script"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn loopback_times_out() {
        let (_port, handle) = spawn_wait("s1", Duration::from_millis(200));
        assert!(handle.await.unwrap().is_err());
    }
}
