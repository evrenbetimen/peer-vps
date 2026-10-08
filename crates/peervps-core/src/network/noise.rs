//! Noise IK handshake that keys a [`TunnelCodec`].
//!
//! `Noise_IK_25519_ChaChaPoly_BLAKE2s` is the same construction WireGuard is
//! built on: the initiator already knows the responder's static key (published
//! with its offer in the DHT), the handshake is one round trip, and both static
//! identities are authenticated. After the handshake we take the two directional
//! cipher keys and hand them to our own UDP codec, which adds compression, an
//! explicit counter and a replay window suitable for a lossy datagram transport.

use snow::{Builder, HandshakeState};

use super::codec::{CodecConfig, TunnelCodec};
use crate::{Error, Result};

pub const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const MAX_MSG: usize = 1024;

/// A node's long-term X25519 identity.
#[derive(Clone)]
pub struct StaticKeypair {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

impl std::fmt::Debug for StaticKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticKeypair").field("public", &hex::encode(&self.public)).finish_non_exhaustive()
    }
}

pub fn generate_keypair() -> Result<StaticKeypair> {
    let kp = builder()?.generate_keypair().map_err(noise_err)?;
    Ok(StaticKeypair { private: kp.private, public: kp.public })
}

fn builder() -> Result<Builder<'static>> {
    Ok(Builder::new(PATTERN.parse().map_err(noise_err)?))
}

fn noise_err(e: snow::Error) -> Error {
    Error::Crypto(format!("noise: {e}"))
}

#[derive(Debug)]
pub struct Initiator {
    hs: HandshakeState,
    session: u32,
}

#[derive(Debug)]
pub struct Responder {
    hs: HandshakeState,
    session: Option<u32>,
}

impl Initiator {
    /// Start a handshake towards `remote_public`. `payload` travels encrypted
    /// in the first message (e.g. the VM id the client wants to reach).
    pub fn start(local: &StaticKeypair, remote_public: &[u8], session: u32, payload: &[u8]) -> Result<(Self, Vec<u8>)> {
        let mut hs = builder()?
            .local_private_key(&local.private)
            .map_err(noise_err)?
            .remote_public_key(remote_public)
            .map_err(noise_err)?
            .build_initiator()
            .map_err(noise_err)?;
        let mut buf = vec![0u8; MAX_MSG];
        let mut msg = session.to_be_bytes().to_vec();
        let n = hs.write_message(payload, &mut buf).map_err(noise_err)?;
        msg.extend_from_slice(&buf[..n]);
        Ok((Self { hs, session }, msg))
    }

    pub fn finish(mut self, response: &[u8], cfg: CodecConfig) -> Result<(TunnelCodec, Vec<u8>)> {
        let mut payload = vec![0u8; MAX_MSG];
        let n = self.hs.read_message(response, &mut payload).map_err(noise_err)?;
        payload.truncate(n);
        let (i2r, r2i) = self.hs.dangerously_get_raw_split();
        Ok((TunnelCodec::new(self.session, i2r, r2i, cfg), payload))
    }
}

impl Responder {
    pub fn new(local: &StaticKeypair) -> Result<Self> {
        let hs =
            builder()?.local_private_key(&local.private).map_err(noise_err)?.build_responder().map_err(noise_err)?;
        Ok(Self { hs, session: None })
    }

    /// Process the initiator's message; returns (initiator static key, payload).
    pub fn read(&mut self, msg: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        if msg.len() < 4 {
            return Err(Error::Crypto("handshake too short".into()));
        }
        let mut payload = vec![0u8; MAX_MSG];
        let n = self.hs.read_message(&msg[4..], &mut payload).map_err(noise_err)?;
        payload.truncate(n);
        self.session = Some(u32::from_be_bytes([msg[0], msg[1], msg[2], msg[3]]));
        let remote =
            self.hs.get_remote_static().ok_or_else(|| Error::Crypto("initiator static key missing".into()))?.to_vec();
        Ok((remote, payload))
    }

    /// Answer a handshake previously accepted with [`Responder::read`].
    pub fn respond(mut self, payload: &[u8], cfg: CodecConfig) -> Result<(TunnelCodec, Vec<u8>)> {
        let session = self.session.ok_or_else(|| Error::Crypto("respond called before read".into()))?;
        let mut buf = vec![0u8; MAX_MSG];
        let n = self.hs.write_message(payload, &mut buf).map_err(noise_err)?;
        buf.truncate(n);
        let (i2r, r2i) = self.hs.dangerously_get_raw_split();
        Ok((TunnelCodec::new(session, r2i, i2r, cfg), buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ik_handshake_keys_both_codecs() {
        let client = generate_keypair().expect("client key");
        let host = generate_keypair().expect("host key");

        let (init, m1) = Initiator::start(&client, &host.public, 42, b"vm:abc").expect("m1");
        let mut resp = Responder::new(&host).expect("responder");
        let (peer, payload) = resp.read(&m1).expect("read m1");
        assert_eq!(peer, client.public);
        assert_eq!(payload, b"vm:abc");
        let (mut host_codec, m2) = resp.respond(b"ok", CodecConfig::default()).expect("m2");
        let (mut client_codec, reply) = init.finish(&m2, CodecConfig::default()).expect("finish");
        assert_eq!(reply, b"ok");

        let dg = client_codec.seal(b"ping").expect("seal");
        assert_eq!(host_codec.open(&dg, 1500).expect("open"), b"ping");
        let dg = host_codec.seal(b"pong").expect("seal");
        assert_eq!(client_codec.open(&dg, 1500).expect("open"), b"pong");
    }

    #[test]
    fn wrong_responder_key_fails() {
        let client = generate_keypair().expect("client");
        let host = generate_keypair().expect("host");
        let imposter = generate_keypair().expect("imposter");
        let (_init, m1) = Initiator::start(&client, &host.public, 1, b"").expect("m1");
        let mut resp = Responder::new(&imposter).expect("responder");
        assert!(resp.read(&m1).is_err());
    }
}
