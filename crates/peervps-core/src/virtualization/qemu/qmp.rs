//! Client for QEMU's machine protocol (QMP) over a loopback TCP socket.
//!
//! TCP rather than a Unix socket so the same code drives QEMU on Linux, macOS
//! and Windows. The socket only listens on 127.0.0.1.

use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::{Error, Result};

#[derive(Debug)]
pub struct Qmp {
    addr: SocketAddr,
    timeout: Duration,
    // QMP serves one client at a time; serialize our own callers.
    lock: Mutex<()>,
}

impl Qmp {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr, timeout: Duration::from_secs(10), lock: Mutex::new(()) }
    }

    /// Run one command and return its `return` value.
    pub async fn execute(&self, command: &str, arguments: Option<Value>) -> Result<Value> {
        let _guard = self.lock.lock().await;
        tokio::time::timeout(self.timeout, self.execute_inner(command, arguments))
            .await
            .map_err(|_| Error::Hypervisor(format!("qmp {command}: timed out")))?
    }

    async fn execute_inner(&self, command: &str, arguments: Option<Value>) -> Result<Value> {
        let stream = TcpStream::connect(self.addr).await?;
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let greeting = next_message(&mut lines).await?;
        if greeting.get("QMP").is_none() {
            return Err(Error::Hypervisor(format!("unexpected qmp greeting: {greeting}")));
        }
        send(&mut wr, &json!({ "execute": "qmp_capabilities" })).await?;
        reply(&mut lines, "qmp_capabilities").await?;
        let mut msg = json!({ "execute": command });
        if let Some(args) = arguments {
            msg["arguments"] = args;
        }
        send(&mut wr, &msg).await?;
        reply(&mut lines, command).await
    }
}

async fn send(wr: &mut tokio::net::tcp::OwnedWriteHalf, msg: &Value) -> Result<()> {
    let mut buf = serde_json::to_vec(msg)?;
    buf.push(b'\n');
    wr.write_all(&buf).await?;
    Ok(())
}

async fn next_message(lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>) -> Result<Value> {
    loop {
        let line = lines.next_line().await?.ok_or_else(|| Error::Hypervisor("qmp connection closed".into()))?;
        if !line.trim().is_empty() {
            return Ok(serde_json::from_str(&line)?);
        }
    }
}

/// Read until the command's reply, skipping asynchronous events.
async fn reply(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    command: &str,
) -> Result<Value> {
    loop {
        let msg = next_message(lines).await?;
        if let Some(ret) = msg.get("return") {
            return Ok(ret.clone());
        }
        if let Some(err) = msg.get("error") {
            let desc = err.get("desc").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(Error::Hypervisor(format!("qmp {command}: {desc}")));
        }
        // {"event": ...}: not ours.
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    /// A fake QMP server answering scripted replies, recording what it receives.
    async fn fake(replies: Vec<&'static str>) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let task = tokio::spawn(async move {
            let (s, _) = listener.accept().await.expect("accept");
            let (rd, mut wr) = s.into_split();
            let mut lines = BufReader::new(rd).lines();
            wr.write_all(b"{\"QMP\": {\"version\": {}, \"capabilities\": []}}\r\n").await.expect("greet");
            let mut seen = Vec::new();
            for r in replies {
                let line = lines.next_line().await.expect("read").expect("line");
                seen.push(line);
                wr.write_all(r.as_bytes()).await.expect("write");
            }
            seen
        });
        (addr, task)
    }

    #[tokio::test]
    async fn negotiates_skips_events_and_returns() {
        let (addr, server) = fake(vec![
            "{\"return\": {}}\r\n",
            "{\"event\": \"STOP\", \"timestamp\": {}}\r\n{\"return\": {\"status\": \"paused\"}}\r\n",
        ])
        .await;
        let ret = Qmp::new(addr).execute("query-status", None).await.expect("execute");
        assert_eq!(ret["status"], "paused");
        let seen = server.await.expect("server");
        assert!(seen[0].contains("qmp_capabilities"));
        assert!(seen[1].contains("query-status"));
    }

    #[tokio::test]
    async fn errors_carry_qemu_description() {
        let (addr, _server) = fake(vec![
            "{\"return\": {}}\r\n",
            "{\"error\": {\"class\": \"GenericError\", \"desc\": \"no such thing\"}}\r\n",
        ])
        .await;
        let err = Qmp::new(addr).execute("migrate", Some(json!({ "uri": "file:/x" }))).await.expect_err("error");
        assert!(err.to_string().contains("no such thing"), "{err}");
    }
}
