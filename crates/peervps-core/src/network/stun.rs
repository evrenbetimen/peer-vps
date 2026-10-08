//! Minimal STUN (RFC 8489) binding client used to learn our public mapping.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use rand::Rng;
use tokio::net::UdpSocket;

use crate::{Error, Result};

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub type TransactionId = [u8; 12];

pub fn binding_request() -> (TransactionId, Vec<u8>) {
    let mut tid = [0u8; 12];
    rand::rng().fill(&mut tid);
    let mut msg = Vec::with_capacity(20);
    msg.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    msg.extend_from_slice(&0u16.to_be_bytes());
    msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg.extend_from_slice(&tid);
    (tid, msg)
}

/// Build a success response; used by the in-process STUN responder that every
/// public PeerVPS node runs for its peers.
pub fn binding_response(tid: &TransactionId, observed: SocketAddr) -> Vec<u8> {
    let mut attr = Vec::new();
    let port = observed.port() ^ (MAGIC_COOKIE >> 16) as u16;
    match observed.ip() {
        IpAddr::V4(ip) => {
            attr.extend_from_slice(&[0, 0x01]);
            attr.extend_from_slice(&port.to_be_bytes());
            attr.extend_from_slice(&(u32::from(ip) ^ MAGIC_COOKIE).to_be_bytes());
        }
        IpAddr::V6(ip) => {
            attr.extend_from_slice(&[0, 0x02]);
            attr.extend_from_slice(&port.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(tid);
            let octets = ip.octets();
            attr.extend((0..16).map(|i| octets[i] ^ mask[i]));
        }
    }
    let mut msg = Vec::with_capacity(20 + 4 + attr.len());
    msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    msg.extend_from_slice(&((4 + attr.len()) as u16).to_be_bytes());
    msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg.extend_from_slice(tid);
    msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    msg.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    msg.extend_from_slice(&attr);
    msg
}

/// Parse a binding success response and return the mapped address.
pub fn parse_binding_response(tid: &TransactionId, msg: &[u8]) -> Result<SocketAddr> {
    let bad = |m: &str| Error::Invalid(format!("stun: {m}"));
    if msg.len() < 20 {
        return Err(bad("short message"));
    }
    if u16::from_be_bytes([msg[0], msg[1]]) != BINDING_SUCCESS {
        return Err(bad("not a binding success"));
    }
    if u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]) != MAGIC_COOKIE {
        return Err(bad("bad magic cookie"));
    }
    if &msg[8..20] != tid {
        return Err(bad("transaction id mismatch"));
    }
    let len = u16::from_be_bytes([msg[2], msg[3]]) as usize;
    let body = msg.get(20..20 + len).ok_or_else(|| bad("truncated body"))?;

    let mut i = 0;
    let mut fallback = None;
    while i + 4 <= body.len() {
        let ty = u16::from_be_bytes([body[i], body[i + 1]]);
        let alen = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
        let v = body.get(i + 4..i + 4 + alen).ok_or_else(|| bad("truncated attribute"))?;
        match ty {
            ATTR_XOR_MAPPED_ADDRESS => return decode_addr(v, Some(tid)),
            ATTR_MAPPED_ADDRESS => fallback = Some(decode_addr(v, None)?),
            _ => {}
        }
        i += 4 + alen.div_ceil(4) * 4;
    }
    fallback.ok_or_else(|| bad("no mapped address"))
}

fn decode_addr(v: &[u8], xor_tid: Option<&TransactionId>) -> Result<SocketAddr> {
    let bad = || Error::Invalid("stun: malformed address".into());
    if v.len() < 4 {
        return Err(bad());
    }
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    if xor_tid.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match v[1] {
        0x01 => {
            let raw: [u8; 4] = v.get(4..8).ok_or_else(bad)?.try_into().map_err(|_| bad())?;
            let mut n = u32::from_be_bytes(raw);
            if xor_tid.is_some() {
                n ^= MAGIC_COOKIE;
            }
            IpAddr::V4(Ipv4Addr::from(n))
        }
        0x02 => {
            let mut raw: [u8; 16] = v.get(4..20).ok_or_else(bad)?.try_into().map_err(|_| bad())?;
            if let Some(tid) = xor_tid {
                let mut mask = [0u8; 16];
                mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                mask[4..].copy_from_slice(tid);
                for (b, m) in raw.iter_mut().zip(mask) {
                    *b ^= m;
                }
            }
            IpAddr::V6(Ipv6Addr::from(raw))
        }
        _ => return Err(bad()),
    };
    Ok(SocketAddr::new(ip, port))
}

/// Ask `server` for our reflexive address, retrying with backoff.
pub async fn query(socket: &UdpSocket, server: SocketAddr, timeout: Duration) -> Result<SocketAddr> {
    let (tid, req) = binding_request();
    let mut buf = [0u8; 512];
    let mut wait = Duration::from_millis(100);
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        socket.send_to(&req, server).await?;
        if let Ok(Ok((n, from))) = tokio::time::timeout(wait, socket.recv_from(&mut buf)).await
            && from == server
            && let Ok(addr) = parse_binding_response(&tid, &buf[..n])
        {
            return Ok(addr);
        }
        wait = (wait * 2).min(Duration::from_secs(1));
    }
    Err(Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "stun query timed out")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_parses_v4_and_v6() {
        for addr in ["203.0.113.7:51820".parse().expect("v4"), "[2001:db8::42]:443".parse().expect("v6")] {
            let (tid, _) = binding_request();
            let resp = binding_response(&tid, addr);
            assert_eq!(parse_binding_response(&tid, &resp).expect("parse"), addr);
        }
    }

    #[tokio::test]
    async fn query_against_local_responder() {
        let server = UdpSocket::bind("127.0.0.1:0").await.expect("bind server");
        let server_addr = server.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).await.expect("recv");
            let tid: TransactionId = buf[8..20].try_into().expect("tid");
            assert!(n >= 20);
            server.send_to(&binding_response(&tid, from), from).await.expect("send");
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.expect("bind client");
        let mapped = query(&client, server_addr, Duration::from_secs(2)).await.expect("query");
        assert_eq!(mapped, client.local_addr().expect("addr"));
    }
}
