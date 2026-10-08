//! Encrypted, mutually authenticated TCP channel between two nodes.
//!
//! `Noise_XX_25519_ChaChaPoly_BLAKE2s`: unlike the overlay's IK handshake,
//! neither side needs to know the other's key in advance, which is what lets
//! a person add a peer by address alone. Both static keys are exchanged and
//! authenticated in the handshake; the caller decides whether to trust them.
//!
//! On the wire every Noise message is a frame with a 2-byte length. The
//! request/response layer sends messages of any size as a 4-byte length
//! followed by the bytes, split across as many frames as it takes; a forwarded
//! port sends one frame per chunk read from its socket.

use std::sync::Arc;

use snow::{Builder, StatelessTransportState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::network::noise::StaticKeypair;
use crate::{Error, Result};

pub const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const MAX_FRAME: usize = 65535;
const TAG: usize = 16;
/// Largest plaintext one frame carries.
pub const MAX_CHUNK: usize = MAX_FRAME - TAG;
/// Largest request or response; offers and instances are a few KiB.
const MAX_MESSAGE: usize = 4 << 20;

fn noise_err(e: snow::Error) -> Error {
    Error::Crypto(format!("noise: {e}"))
}

fn builder(local: &StaticKeypair) -> Result<Builder<'_>> {
    Builder::new(PATTERN.parse().map_err(noise_err)?).local_private_key(&local.private).map_err(noise_err)
}

async fn write_frame(w: &mut (impl AsyncWriteExt + Unpin), frame: &[u8]) -> Result<()> {
    let len = u16::try_from(frame.len()).map_err(|_| Error::Invalid("frame too large".into()))?;
    let mut buf = Vec::with_capacity(frame.len() + 2);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(frame);
    w.write_all(&buf).await?;
    Ok(())
}

/// `None` on a clean end of stream.
async fn read_frame(r: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let mut frame = vec![0u8; usize::from(u16::from_be_bytes(len))];
    r.read_exact(&mut frame).await?;
    Ok(Some(frame))
}

/// An established channel; `remote` is the other node's static public key.
#[derive(Debug)]
pub struct Channel {
    pub remote: Vec<u8>,
    pub tx: Sender,
    pub rx: Receiver,
}

#[derive(Debug)]
pub struct Sender {
    w: OwnedWriteHalf,
    t: Arc<StatelessTransportState>,
    nonce: u64,
}

#[derive(Debug)]
pub struct Receiver {
    r: OwnedReadHalf,
    t: Arc<StatelessTransportState>,
    nonce: u64,
    /// Plaintext received but not yet consumed by [`Receiver::recv`].
    pending: Vec<u8>,
}

impl Channel {
    /// Dial side of the handshake.
    pub async fn connect(stream: TcpStream, local: &StaticKeypair) -> Result<Self> {
        stream.set_nodelay(true)?;
        let (mut r, mut w) = stream.into_split();
        let mut hs = builder(local)?.build_initiator().map_err(noise_err)?;
        let mut buf = vec![0u8; MAX_FRAME];
        let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
        write_frame(&mut w, &buf[..n]).await?;
        let m2 = read_frame(&mut r).await?.ok_or_else(|| Error::Crypto("peer closed during handshake".into()))?;
        hs.read_message(&m2, &mut buf).map_err(noise_err)?;
        let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
        write_frame(&mut w, &buf[..n]).await?;
        let remote = hs.get_remote_static().ok_or_else(|| Error::Crypto("peer sent no static key".into()))?.to_vec();
        Self::finish(remote, hs.into_stateless_transport_mode().map_err(noise_err)?, r, w)
    }

    /// Listen side of the handshake.
    pub async fn accept(stream: TcpStream, local: &StaticKeypair) -> Result<Self> {
        stream.set_nodelay(true)?;
        let (mut r, mut w) = stream.into_split();
        let mut hs = builder(local)?.build_responder().map_err(noise_err)?;
        let mut buf = vec![0u8; MAX_FRAME];
        let m1 = read_frame(&mut r).await?.ok_or_else(|| Error::Crypto("peer closed during handshake".into()))?;
        hs.read_message(&m1, &mut buf).map_err(noise_err)?;
        let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
        write_frame(&mut w, &buf[..n]).await?;
        let m3 = read_frame(&mut r).await?.ok_or_else(|| Error::Crypto("peer closed during handshake".into()))?;
        hs.read_message(&m3, &mut buf).map_err(noise_err)?;
        let remote = hs.get_remote_static().ok_or_else(|| Error::Crypto("peer sent no static key".into()))?.to_vec();
        Self::finish(remote, hs.into_stateless_transport_mode().map_err(noise_err)?, r, w)
    }

    fn finish(remote: Vec<u8>, t: StatelessTransportState, r: OwnedReadHalf, w: OwnedWriteHalf) -> Result<Self> {
        let t = Arc::new(t);
        Ok(Self {
            remote,
            tx: Sender { w, t: t.clone(), nonce: 0 },
            rx: Receiver { r, t, nonce: 0, pending: Vec::new() },
        })
    }

    pub async fn send(&mut self, msg: &[u8]) -> Result<()> {
        self.tx.send(msg).await
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        self.rx.recv().await
    }
}

impl Sender {
    /// Encrypt and send up to [`MAX_CHUNK`] bytes as one frame.
    pub async fn send_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        let mut buf = vec![0u8; chunk.len() + TAG];
        let n = self.t.write_message(self.nonce, chunk, &mut buf).map_err(noise_err)?;
        self.nonce += 1;
        write_frame(&mut self.w, &buf[..n]).await
    }

    /// Send one length-prefixed message.
    pub async fn send(&mut self, msg: &[u8]) -> Result<()> {
        let len = u32::try_from(msg.len()).map_err(|_| Error::Invalid("message too large".into()))?;
        let mut data = len.to_be_bytes().to_vec();
        data.extend_from_slice(msg);
        for chunk in data.chunks(MAX_CHUNK) {
            self.send_chunk(chunk).await?;
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let _ = self.w.shutdown().await;
    }
}

impl Receiver {
    /// Next decrypted frame; `None` when the other side closed the stream.
    pub async fn recv_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.pending.is_empty() {
            return Ok(Some(std::mem::take(&mut self.pending)));
        }
        let Some(frame) = read_frame(&mut self.r).await? else { return Ok(None) };
        let mut out = vec![0u8; frame.len()];
        let n = self.t.read_message(self.nonce, &frame, &mut out).map_err(noise_err)?;
        self.nonce += 1;
        out.truncate(n);
        Ok(Some(out))
    }

    /// Next length-prefixed message.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            if data.len() >= 4 {
                let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
                if len > MAX_MESSAGE {
                    return Err(Error::Invalid(format!("peer message of {len} bytes")));
                }
                if data.len() >= 4 + len {
                    // Anything after the message is the start of a forwarded stream.
                    self.pending = data.split_off(4 + len);
                    data.drain(..4);
                    return Ok(data);
                }
            }
            match self.recv_chunk().await? {
                Some(chunk) => data.extend_from_slice(&chunk),
                None => return Err(Error::Io(std::io::ErrorKind::UnexpectedEof.into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::noise::generate_keypair;

    #[tokio::test]
    async fn xx_handshake_authenticates_both_keys_and_carries_large_messages() {
        let (a, b) = (generate_keypair().expect("a"), generate_keypair().expect("b"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let b2 = b.clone();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.expect("accept");
            let mut ch = Channel::accept(s, &b2).await.expect("accept handshake");
            let msg = ch.recv().await.expect("recv");
            ch.send(&msg).await.expect("echo");
            ch.remote
        });
        let mut ch = Channel::connect(TcpStream::connect(addr).await.expect("dial"), &a).await.expect("handshake");
        assert_eq!(ch.remote, b.public);
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        ch.send(&big).await.expect("send");
        assert_eq!(ch.recv().await.expect("recv"), big);
        assert_eq!(server.await.expect("server"), a.public);
    }
}
