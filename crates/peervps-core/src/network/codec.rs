//! Tunnel datagram codec: zstd compression + ChaCha20-Poly1305 AEAD.
//!
//! Wire format (all integers big-endian):
//!
//! ```text
//! +---------+-------+------------+-------------+---------------------------+
//! | ver: u8 | flags | session u32| counter u64 | AEAD(ciphertext ‖ tag 16) |
//! +---------+-------+------------+-------------+---------------------------+
//! ```
//!
//! The 14-byte header is authenticated as associated data. The nonce is
//! `0u32 ‖ counter`, so each direction must use its own key (the Noise
//! handshake's split provides exactly that). Compression happens *before*
//! encryption and only when it actually shrinks the packet; encrypted data
//! does not compress. A 64-packet sliding window rejects replays while still
//! tolerating UDP reordering.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::{Error, Result};

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 14;
pub const TAG_LEN: usize = 16;
const FLAG_ZSTD: u8 = 0b0000_0001;
/// Packets smaller than this are never worth a compression attempt.
const MIN_COMPRESS_LEN: usize = 96;
const REPLAY_WINDOW: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecConfig {
    /// zstd level; 1–3 keeps per-packet latency in the low microseconds.
    pub zstd_level: i32,
    pub compress: bool,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self { zstd_level: 1, compress: true }
    }
}

/// One direction-pair of a tunnel session.
pub struct TunnelCodec {
    session: u32,
    tx: ChaCha20Poly1305,
    rx: ChaCha20Poly1305,
    tx_counter: u64,
    rx_highest: u64,
    rx_window: u64,
    rx_seen_any: bool,
    cfg: CodecConfig,
}

impl std::fmt::Debug for TunnelCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelCodec")
            .field("session", &self.session)
            .field("tx_counter", &self.tx_counter)
            .field("rx_highest", &self.rx_highest)
            .finish_non_exhaustive()
    }
}

impl TunnelCodec {
    pub fn new(session: u32, tx_key: [u8; 32], rx_key: [u8; 32], cfg: CodecConfig) -> Self {
        Self {
            session,
            tx: ChaCha20Poly1305::new(Key::from_slice(&tx_key)),
            rx: ChaCha20Poly1305::new(Key::from_slice(&rx_key)),
            tx_counter: 0,
            rx_highest: 0,
            rx_window: 0,
            rx_seen_any: false,
            cfg,
        }
    }

    pub fn session(&self) -> u32 {
        self.session
    }

    /// Compress (if useful) and encrypt one IP packet into a datagram.
    pub fn seal(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        let counter = self.tx_counter;
        self.tx_counter =
            counter.checked_add(1).ok_or_else(|| Error::Crypto("tx counter exhausted; rekey required".into()))?;

        let mut flags = 0u8;
        let compressed;
        let body: &[u8] = if self.cfg.compress && packet.len() >= MIN_COMPRESS_LEN {
            compressed = zstd::bulk::compress(packet, self.cfg.zstd_level)?;
            if compressed.len() < packet.len() {
                flags |= FLAG_ZSTD;
                &compressed
            } else {
                packet
            }
        } else {
            packet
        };

        let header = encode_header(flags, self.session, counter);
        let ct = self
            .tx
            .encrypt(&nonce(counter), Payload { msg: body, aad: &header })
            .map_err(|_| Error::Crypto("encrypt".into()))?;
        let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Authenticate, replay-check, decrypt and decompress a datagram.
    pub fn open(&mut self, datagram: &[u8], max_packet: usize) -> Result<Vec<u8>> {
        if datagram.len() < HEADER_LEN + TAG_LEN {
            return Err(Error::Crypto("datagram too short".into()));
        }
        let (header, ct) = datagram.split_at(HEADER_LEN);
        let (ver, flags, session, counter) = decode_header(header);
        if ver != VERSION {
            return Err(Error::Crypto(format!("unsupported version {ver}")));
        }
        if session != self.session {
            return Err(Error::Crypto("session mismatch".into()));
        }
        if !self.replay_ok(counter) {
            return Err(Error::Crypto(format!("replayed or stale counter {counter}")));
        }
        let body = self
            .rx
            .decrypt(&nonce(counter), Payload { msg: ct, aad: header })
            .map_err(|_| Error::Crypto("authentication failed".into()))?;
        // Only mark the counter as seen once the tag verified.
        self.replay_mark(counter);

        if flags & FLAG_ZSTD != 0 { Ok(zstd::bulk::decompress(&body, max_packet)?) } else { Ok(body) }
    }

    fn replay_ok(&self, counter: u64) -> bool {
        if !self.rx_seen_any || counter > self.rx_highest {
            return true;
        }
        let age = self.rx_highest - counter;
        age < REPLAY_WINDOW && self.rx_window & (1 << age) == 0
    }

    fn replay_mark(&mut self, counter: u64) {
        if !self.rx_seen_any {
            self.rx_seen_any = true;
            self.rx_highest = counter;
            self.rx_window = 1;
        } else if counter > self.rx_highest {
            let shift = counter - self.rx_highest;
            self.rx_window = if shift >= REPLAY_WINDOW { 0 } else { self.rx_window << shift };
            self.rx_window |= 1;
            self.rx_highest = counter;
        } else {
            self.rx_window |= 1 << (self.rx_highest - counter);
        }
    }
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    *Nonce::from_slice(&n)
}

fn encode_header(flags: u8, session: u32, counter: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0] = VERSION;
    h[1] = flags;
    h[2..6].copy_from_slice(&session.to_be_bytes());
    h[6..14].copy_from_slice(&counter.to_be_bytes());
    h
}

fn decode_header(h: &[u8]) -> (u8, u8, u32, u64) {
    let mut s = [0u8; 4];
    s.copy_from_slice(&h[2..6]);
    let mut c = [0u8; 8];
    c.copy_from_slice(&h[6..14]);
    (h[0], h[1], u32::from_be_bytes(s), u64::from_be_bytes(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (TunnelCodec, TunnelCodec) {
        let (k1, k2) = ([1u8; 32], [2u8; 32]);
        (TunnelCodec::new(9, k1, k2, CodecConfig::default()), TunnelCodec::new(9, k2, k1, CodecConfig::default()))
    }

    #[test]
    fn roundtrip_compresses_redundant_payloads() {
        let (mut a, mut b) = pair();
        let packet = vec![0x45u8; 1400];
        let dg = a.seal(&packet).expect("seal");
        assert!(dg.len() < 200, "1400 repeated bytes should shrink, got {}", dg.len());
        assert_eq!(dg[1] & FLAG_ZSTD, FLAG_ZSTD);
        assert_eq!(b.open(&dg, 65_535).expect("open"), packet);
    }

    #[test]
    fn incompressible_payload_is_sent_raw() {
        let (mut a, mut b) = pair();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let packet: Vec<u8> = (0..1200)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let dg = a.seal(&packet).expect("seal");
        assert_eq!(dg[1] & FLAG_ZSTD, 0);
        assert_eq!(b.open(&dg, 65_535).expect("open"), packet);
    }

    #[test]
    fn rejects_tampering_and_replay_but_allows_reordering() {
        let (mut a, mut b) = pair();
        let d0 = a.seal(b"zero").expect("seal");
        let d1 = a.seal(b"one").expect("seal");
        let d2 = a.seal(b"two").expect("seal");

        let mut bad = d2.clone();
        *bad.last_mut().expect("non-empty") ^= 0xff;
        assert!(b.open(&bad, 1500).is_err());

        assert_eq!(b.open(&d2, 1500).expect("d2"), b"two");
        assert_eq!(b.open(&d0, 1500).expect("late d0"), b"zero");
        assert!(b.open(&d0, 1500).is_err(), "replay");
        assert_eq!(b.open(&d1, 1500).expect("late d1"), b"one");
    }
}
