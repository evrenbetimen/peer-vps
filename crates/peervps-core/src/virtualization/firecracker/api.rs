//! Minimal HTTP/1.1 client for the Firecracker API socket.
//!
//! Firecracker serves a small REST API on a Unix socket. Requests are tiny,
//! one at a time, and responses are `204 No Content` or a short JSON body, so a
//! hand-rolled client keeps us off a full HTTP stack for this one use.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::{Error, Result};

const MAX_RESPONSE: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct ApiClient {
    socket: PathBuf,
    timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: String,
}

impl ApiClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into(), timeout: Duration::from_secs(10) }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn put(&self, path: &str, body: &impl Serialize) -> Result<()> {
        self.expect_ok("PUT", path, Some(&serde_json::to_vec(body)?)).await
    }

    pub async fn patch(&self, path: &str, body: &impl Serialize) -> Result<()> {
        self.expect_ok("PATCH", path, Some(&serde_json::to_vec(body)?)).await
    }

    pub async fn get(&self, path: &str) -> Result<ApiResponse> {
        self.request("GET", path, None).await
    }

    async fn expect_ok(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<()> {
        let resp = self.request(method, path, body).await?;
        if (200..300).contains(&resp.status) {
            return Ok(());
        }
        // Firecracker reports errors as {"fault_message": "..."}.
        let fault = serde_json::from_str::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|v| v.get("fault_message").and_then(|m| m.as_str()).map(str::to_owned))
            .unwrap_or(resp.body);
        Err(Error::Hypervisor(format!("firecracker {method} {path} -> {}: {fault}", resp.status)))
    }

    pub async fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<ApiResponse> {
        tokio::time::timeout(self.timeout, self.request_inner(method, path, body))
            .await
            .map_err(|_| Error::Hypervisor(format!("firecracker {method} {path}: timed out")))?
    }

    async fn request_inner(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<ApiResponse> {
        let mut stream = UnixStream::connect(&self.socket).await?;
        let body = body.unwrap_or_default();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if !body.is_empty() {
            req.push_str("Content-Type: application/json\r\n");
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).await?;
        stream.write_all(body).await?;
        stream.flush().await?;
        read_response(&mut stream).await
    }
}

async fn read_response(stream: &mut UnixStream) -> Result<ApiResponse> {
    let bad = |m: &str| Error::Hypervisor(format!("firecracker api: {m}"));
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 4096];
    // Read until headers are complete.
    let header_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(bad("connection closed before headers"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_RESPONSE {
            return Err(bad("response too large"));
        }
    };
    let head = std::str::from_utf8(&buf[..header_end]).map_err(|_| bad("non-utf8 headers"))?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("malformed status line"))?;
    let content_length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok());

    let mut body = buf[header_end..].to_vec();
    match content_length {
        Some(len) if len > MAX_RESPONSE => return Err(bad("response too large")),
        Some(len) => {
            while body.len() < len {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    return Err(bad("connection closed mid-body"));
                }
                body.extend_from_slice(&chunk[..n]);
            }
            body.truncate(len);
        }
        // 204 and friends carry no body; otherwise read to EOF (we sent Connection: close).
        None if status == 204 || status == 304 || (100..200).contains(&status) => body.clear(),
        None => {
            stream.take(MAX_RESPONSE as u64).read_to_end(&mut body).await?;
        }
    }
    Ok(ApiResponse { status, body: String::from_utf8_lossy(&body).into_owned() })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;

    use super::*;

    async fn serve_once(listener: UnixListener, response: &'static str) -> String {
        let (mut s, _) = listener.accept().await.expect("accept");
        let mut req = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = s.read(&mut chunk).await.expect("read");
            req.extend_from_slice(&chunk[..n]);
            if let Some(i) = find(&req, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&req[..i]).to_string();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if req.len() >= i + 4 + len {
                    break;
                }
            }
        }
        s.write_all(response.as_bytes()).await.expect("write");
        String::from_utf8_lossy(&req).into_owned()
    }

    fn sock() -> PathBuf {
        std::env::temp_dir().join(format!("pvps-fc-api-{}.sock", uuid::Uuid::new_v4().simple()))
    }

    #[tokio::test]
    async fn put_sends_json_and_accepts_204() {
        let path = sock();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = tokio::spawn(serve_once(listener, "HTTP/1.1 204 No Content\r\nServer: Firecracker API\r\n\r\n"));
        ApiClient::new(&path).put("/actions", &serde_json::json!({"action_type": "InstanceStart"})).await.expect("put");
        let req = server.await.expect("server");
        assert!(req.starts_with("PUT /actions HTTP/1.1\r\n"));
        assert!(req.ends_with(r#"{"action_type":"InstanceStart"}"#));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn fault_message_becomes_error() {
        let path = sock();
        let listener = UnixListener::bind(&path).expect("bind");
        let body = r#"{"fault_message":"Invalid kernel path"}"#;
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_boxed_str(),
        );
        tokio::spawn(serve_once(listener, resp));
        let err = ApiClient::new(&path).put("/boot-source", &serde_json::json!({})).await.expect_err("400");
        assert!(err.to_string().contains("Invalid kernel path"), "{err}");
        let _ = std::fs::remove_file(path);
    }
}
