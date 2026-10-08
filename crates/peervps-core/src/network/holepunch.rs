//! Aggressive UDP hole punching.
//!
//! Both sides learn each other's candidates (LAN address, STUN reflexive
//! address) through the rendezvous/DHT and then call [`punch`] at roughly the
//! same moment. Each side bursts small probes at every candidate so both NATs
//! open outbound mappings; the first authenticated probe or ack that arrives
//! wins and fixes the path.
//!
//! For symmetric NATs (mapping changes per destination) the reflexive port seen
//! by STUN is only a hint, so [`PunchConfig::port_spread`] additionally sprays
//! the ports just above it, which catches the common sequential allocators.
//! When nothing gets through, the caller falls back to a TURN-style relay node.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::{Error, Result};

const PROBE: &[u8; 8] = b"PVPSPNCH";
const ACK: &[u8; 8] = b"PVPSPACK";

#[derive(Debug, Clone, Copy)]
pub struct PunchConfig {
    pub interval: Duration,
    pub timeout: Duration,
    /// Extra consecutive ports to try above each reflexive candidate.
    pub port_spread: u16,
}

impl Default for PunchConfig {
    fn default() -> Self {
        Self { interval: Duration::from_millis(20), timeout: Duration::from_secs(5), port_spread: 0 }
    }
}

fn frame(kind: &[u8; 8], token: &[u8; 16]) -> [u8; 24] {
    let mut f = [0u8; 24];
    f[..8].copy_from_slice(kind);
    f[8..].copy_from_slice(token);
    f
}

fn expand(candidates: &[SocketAddr], spread: u16) -> Vec<SocketAddr> {
    let mut out = BTreeSet::new();
    for c in candidates {
        for d in 0..=spread {
            if let Some(port) = c.port().checked_add(d) {
                out.insert(SocketAddr::new(c.ip(), port));
            }
        }
    }
    out.into_iter().collect()
}

/// Punch towards `candidates`; `token` is a per-session secret both sides got
/// from the rendezvous so stray traffic cannot hijack the path. Returns the
/// remote address that answered.
pub async fn punch(
    socket: &UdpSocket,
    candidates: &[SocketAddr],
    token: [u8; 16],
    cfg: PunchConfig,
) -> Result<SocketAddr> {
    if candidates.is_empty() {
        return Err(Error::Invalid("no candidates to punch".into()));
    }
    let targets = expand(candidates, cfg.port_spread);
    let probe = frame(PROBE, &token);
    let ack = frame(ACK, &token);
    let mut ticker = tokio::time::interval(cfg.interval);
    let deadline = tokio::time::sleep(cfg.timeout);
    tokio::pin!(deadline);
    let mut buf = [0u8; 64];

    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "hole punching timed out")));
            }
            _ = ticker.tick() => {
                for t in &targets {
                    // Unreachable candidates are expected; keep spraying the rest.
                    let _ = socket.send_to(&probe, t).await;
                }
            }
            r = socket.recv_from(&mut buf) => {
                let (n, from) = match r {
                    Ok(v) => v,
                    // Some stacks surface ICMP unreachable from a dead candidate here.
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => continue,
                    Err(e) => return Err(e.into()),
                };
                if n == 24 && buf[8..24] == token {
                    if buf[..8] == *PROBE {
                        // Answer so the peer can stop too; it may not have seen our probes yet.
                        socket.send_to(&ack, from).await?;
                        return Ok(from);
                    }
                    if buf[..8] == *ACK {
                        return Ok(from);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_peers_find_each_other() {
        let a = UdpSocket::bind("127.0.0.1:0").await.expect("a");
        let b = UdpSocket::bind("127.0.0.1:0").await.expect("b");
        let (aa, ba) = (a.local_addr().expect("a"), b.local_addr().expect("b"));
        let token = [5u8; 16];
        // b advertises a wrong reflexive port too; a must still find the right one.
        let bogus: SocketAddr = "127.0.0.1:9".parse().expect("addr");
        let (a_candidates, b_candidates) = ([bogus, ba], [aa]);
        let (ra, rb) = tokio::join!(
            punch(&a, &a_candidates, token, PunchConfig::default()),
            punch(&b, &b_candidates, token, PunchConfig::default()),
        );
        assert_eq!(ra.expect("a punched"), ba);
        assert_eq!(rb.expect("b punched"), aa);
    }

    #[test]
    fn spread_expands_ports() {
        let c: SocketAddr = "198.51.100.1:40000".parse().expect("addr");
        assert_eq!(expand(&[c], 3).len(), 4);
    }
}
