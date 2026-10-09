//! Connecting two machines that cannot reach each other directly.
//!
//! When neither side can accept connections (both behind a provider's CGNAT),
//! a third machine that can, the **relay**, joins them. A node keeps one
//! control connection to its relay; someone who wants to reach it dials the
//! relay instead and names the node. The relay asks the node to dial in for
//! that call, then splices the two TCP connections together.
//!
//! The relay is not trusted with anything: its own Noise channel to each side
//! only carries these few control messages, and once spliced the two nodes run
//! their normal end-to-end Noise handshake through it, so the relay only ever
//! sees ciphertext and cannot pose as either node. It does learn who talks to
//! whom and how much.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::Identity;
use super::channel::Channel;
use crate::network::noise::StaticKeypair;
use crate::{Error, Result};

/// Port `peervps relay` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7073;
const ANSWER_WITHIN: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(20);
const MAX_NODES: usize = 1024;
/// Calls waiting for their node to dial in, per relay.
const MAX_WAITING: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum Req {
    /// Keep this connection open and tell me when someone calls.
    Register,
    /// Join me to this node.
    Connect {
        to: String,
    },
    /// I am dialing in for this call.
    Accept {
        token: String,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum Resp {
    Registered {
        id: String,
    },
    Incoming {
        token: String,
    },
    /// From the next byte on, this connection is spliced to the other side.
    Ready,
    Pong,
    Error {
        message: String,
    },
}

async fn send(ch: &mut Channel, msg: &impl Serialize) -> Result<()> {
    ch.send(&serde_json::to_vec(msg)?).await
}

async fn recv<T: for<'de> Deserialize<'de>>(ch: &mut Channel) -> Result<T> {
    Ok(serde_json::from_slice(&ch.recv().await?)?)
}

fn relay_error(m: String) -> Error {
    Error::Peer(format!("relay: {m}"))
}

#[derive(Default)]
struct State {
    /// Registered nodes: id -> where to post incoming calls.
    nodes: HashMap<String, mpsc::Sender<String>>,
    /// Calls waiting for their node: token -> where its connection goes.
    waiting: HashMap<String, oneshot::Sender<TcpStream>>,
}

/// Run a relay on `addr`; returns the bound address and the accept loop.
pub async fn serve(addr: SocketAddr, identity: Identity) -> Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let state = Arc::new(Mutex::new(State::default()));
    let key = Arc::new(identity.keypair);
    tracing::info!(%bound, id = %identity.id, "relaying for peers");
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, from)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let (state, key) = (state.clone(), key.clone());
            tokio::spawn(async move {
                if let Err(e) = handle(stream, &key, state).await {
                    tracing::debug!(%from, error = %e, "relay connection ended");
                }
            });
        }
    });
    Ok((bound, task))
}

async fn handle(stream: TcpStream, key: &StaticKeypair, state: Arc<Mutex<State>>) -> Result<()> {
    let mut ch = tokio::time::timeout(ANSWER_WITHIN, Channel::accept(stream, key))
        .await
        .map_err(|_| relay_error("handshake timed out".into()))??;
    let who = Identity::id_for(&ch.remote);
    let req: Req =
        tokio::time::timeout(ANSWER_WITHIN, recv(&mut ch)).await.map_err(|_| relay_error("no request".into()))??;
    match req {
        Req::Register => register(ch, who, state).await,
        Req::Connect { to } => {
            let (token, rx) = {
                let mut s = state.lock().await;
                let Some(node) = s.nodes.get(&to).cloned() else {
                    drop(s);
                    return send(&mut ch, &Resp::Error { message: format!("{to} is not connected to this relay") })
                        .await;
                };
                if s.waiting.len() >= MAX_WAITING {
                    drop(s);
                    return send(&mut ch, &Resp::Error { message: "relay is busy".into() }).await;
                }
                let token = uuid::Uuid::new_v4().simple().to_string();
                let (tx, rx) = oneshot::channel();
                s.waiting.insert(token.clone(), tx);
                // A full or closed queue means the node is gone; the wait below times out.
                let _ = node.try_send(token.clone());
                (token, rx)
            };
            let theirs = tokio::time::timeout(ANSWER_WITHIN, rx).await;
            state.lock().await.waiting.remove(&token);
            let Ok(Ok(mut theirs)) = theirs else {
                return send(&mut ch, &Resp::Error { message: format!("{to} did not answer") }).await;
            };
            send(&mut ch, &Resp::Ready).await?;
            let mut mine = ch.into_stream()?;
            tracing::debug!(from = %who, %to, "relaying");
            let _ = tokio::io::copy_bidirectional(&mut mine, &mut theirs).await;
            Ok(())
        }
        Req::Accept { token } => {
            let Some(waiting) = state.lock().await.waiting.remove(&token) else {
                return send(&mut ch, &Resp::Error { message: "no such call".into() }).await;
            };
            send(&mut ch, &Resp::Ready).await?;
            let _ = waiting.send(ch.into_stream()?);
            Ok(())
        }
        Req::Ping => send(&mut ch, &Resp::Pong).await,
    }
}

/// Hold a node's control connection until it drops.
async fn register(mut ch: Channel, who: String, state: Arc<Mutex<State>>) -> Result<()> {
    let (tx, mut calls) = mpsc::channel::<String>(16);
    {
        let mut s = state.lock().await;
        if s.nodes.len() >= MAX_NODES && !s.nodes.contains_key(&who) {
            drop(s);
            return send(&mut ch, &Resp::Error { message: "relay is full".into() }).await;
        }
        // A reconnect replaces the old registration.
        s.nodes.insert(who.clone(), tx.clone());
    }
    send(&mut ch, &Resp::Registered { id: who.clone() }).await?;
    let Channel { tx: mut out, rx: mut inbox, .. } = ch;
    // Reads in their own task: a cancelled read would lose its frame.
    let (seen_tx, mut seen) = mpsc::channel::<Result<Req>>(4);
    let reader = tokio::spawn(async move {
        loop {
            let msg = inbox.recv().await.and_then(|b| Ok(serde_json::from_slice::<Req>(&b)?));
            let failed = msg.is_err();
            if seen_tx.send(msg).await.is_err() || failed {
                break;
            }
        }
    });
    let result: Result<()> = async {
        loop {
            tokio::select! {
                Some(token) = calls.recv() => {
                    out.send(&serde_json::to_vec(&Resp::Incoming { token })?).await?;
                }
                msg = seen.recv() => match msg {
                    Some(Ok(Req::Ping)) => out.send(&serde_json::to_vec(&Resp::Pong)?).await?,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e),
                    None => return Ok(()),
                },
                _ = tokio::time::sleep(PING_EVERY * 3) => return Err(relay_error(format!("{who} went quiet"))),
            }
        }
    }
    .await;
    reader.abort();
    let mut s = state.lock().await;
    if s.nodes.get(&who).is_some_and(|t| t.same_channel(&tx)) {
        s.nodes.remove(&who);
    }
    result
}

// ---- node side ----

/// Strip `relay://` from a peer address; `None` for a direct address.
pub fn relay_of(address: &str) -> Option<&str> {
    address.strip_prefix("relay://")
}

async fn dial_relay(relay: &str, key: &StaticKeypair) -> Result<Channel> {
    let stream = tokio::time::timeout(ANSWER_WITHIN, TcpStream::connect(relay))
        .await
        .map_err(|_| relay_error(format!("{relay} timed out")))?
        .map_err(|e| relay_error(format!("cannot reach {relay}: {e}")))?;
    tokio::time::timeout(ANSWER_WITHIN, Channel::connect(stream, key))
        .await
        .map_err(|_| relay_error(format!("{relay}: handshake timed out")))?
}

async fn until_ready(ch: &mut Channel) -> Result<()> {
    match tokio::time::timeout(ANSWER_WITHIN * 2, recv::<Resp>(ch)).await {
        Ok(Ok(Resp::Ready)) => Ok(()),
        Ok(Ok(Resp::Error { message })) => Err(relay_error(message)),
        Ok(Ok(other)) => Err(relay_error(format!("unexpected answer {other:?}"))),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(relay_error("no answer".into())),
    }
}

/// A raw connection to `to` through `relay`, ready for the end-to-end handshake.
pub async fn connect(relay: &str, to: &str, key: &StaticKeypair) -> Result<TcpStream> {
    let mut ch = dial_relay(relay, key).await?;
    send(&mut ch, &Req::Connect { to: to.to_owned() }).await?;
    until_ready(&mut ch).await?;
    ch.into_stream()
}

/// What a node's registration at its relay looks like right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RelayState {
    Off,
    Connecting,
    Connected,
    /// Lost or refused; retrying.
    Retrying,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayStatus {
    pub state: RelayState,
    /// `host:port` of the relay.
    pub address: Option<String>,
    pub detail: Option<String>,
}

impl RelayStatus {
    pub fn off() -> Self {
        Self { state: RelayState::Off, address: None, detail: None }
    }
}

/// Stay registered at `relay`, handing each incoming call's connection to
/// `incoming`, and reporting progress through `status`. Reconnects forever.
pub async fn stay_registered(
    relay: String,
    key: StaticKeypair,
    status: impl Fn(RelayStatus) + Send + Sync + 'static,
    incoming: mpsc::Sender<TcpStream>,
) {
    let say = |state, detail: Option<String>| status(RelayStatus { state, address: Some(relay.clone()), detail });
    let mut backoff = Duration::from_secs(1);
    loop {
        say(RelayState::Connecting, None);
        let err = match registered_session(&relay, &key, &say, &incoming).await {
            Ok(()) => "relay closed the connection".to_owned(),
            Err(e) => e.to_string(),
        };
        tracing::warn!(%relay, error = %err, "relay registration lost; retrying");
        say(RelayState::Retrying, Some(err));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn registered_session(
    relay: &str,
    key: &StaticKeypair,
    say: &(impl Fn(RelayState, Option<String>) + Sync),
    incoming: &mpsc::Sender<TcpStream>,
) -> Result<()> {
    let mut ch = dial_relay(relay, key).await?;
    send(&mut ch, &Req::Register).await?;
    match tokio::time::timeout(ANSWER_WITHIN, recv::<Resp>(&mut ch)).await {
        Ok(Ok(Resp::Registered { .. })) => {}
        Ok(Ok(Resp::Error { message })) => return Err(relay_error(message)),
        Ok(Ok(other)) => return Err(relay_error(format!("unexpected answer {other:?}"))),
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(relay_error("no answer to registration".into())),
    }
    say(RelayState::Connected, None);
    let Channel { tx: mut out, rx: mut inbox, .. } = ch;
    let (seen_tx, mut seen) = mpsc::channel::<Result<Resp>>(4);
    let reader = tokio::spawn(async move {
        loop {
            let msg = inbox.recv().await.and_then(|b| Ok(serde_json::from_slice::<Resp>(&b)?));
            let failed = msg.is_err();
            if seen_tx.send(msg).await.is_err() || failed {
                break;
            }
        }
    });
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut last_heard = tokio::time::Instant::now();
    let result = loop {
        tokio::select! {
            _ = ping.tick() => {
                if last_heard.elapsed() > PING_EVERY * 3 {
                    break Err(relay_error(format!("{relay} went quiet")));
                }
                if let Err(e) = out.send(&serde_json::to_vec(&Req::Ping)?).await {
                    break Err(e);
                }
            }
            msg = seen.recv() => match msg {
                Some(Ok(Resp::Incoming { token })) => {
                    last_heard = tokio::time::Instant::now();
                    let (relay, key, incoming) = (relay.to_owned(), key.clone(), incoming.clone());
                    tokio::spawn(async move {
                        match answer(&relay, &key, token).await {
                            Ok(stream) => {
                                let _ = incoming.send(stream).await;
                            }
                            Err(e) => tracing::warn!(%relay, error = %e, "could not answer a relayed call"),
                        }
                    });
                }
                Some(Ok(_)) => last_heard = tokio::time::Instant::now(),
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            },
        }
    };
    reader.abort();
    result
}

async fn answer(relay: &str, key: &StaticKeypair, token: String) -> Result<TcpStream> {
    let mut ch = dial_relay(relay, key).await?;
    send(&mut ch, &Req::Accept { token }).await?;
    until_ready(&mut ch).await?;
    ch.into_stream()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_relay_splices_a_call_and_sees_only_the_inner_handshake() {
        let (addr, server) =
            serve("127.0.0.1:0".parse().expect("addr"), Identity::generate().expect("relay")).await.expect("relay");
        let relay = addr.to_string();
        let (a, b) = (Identity::generate().expect("a"), Identity::generate().expect("b"));

        let (calls_tx, mut calls) = mpsc::channel(4);
        let states = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = states.clone();
        let reg = tokio::spawn(stay_registered(
            relay.clone(),
            b.keypair.clone(),
            move |s| s2.lock().expect("lock").push(s.state),
            calls_tx,
        ));
        for _ in 0..50 {
            if states.lock().expect("lock").contains(&RelayState::Connected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(states.lock().expect("lock").contains(&RelayState::Connected));

        let b_key = b.keypair.clone();
        let host = tokio::spawn(async move {
            let stream = calls.recv().await.expect("a call");
            let mut ch = Channel::accept(stream, &b_key).await.expect("end-to-end handshake");
            let msg = ch.recv().await.expect("recv");
            ch.send(&msg).await.expect("echo");
            ch.remote
        });
        let stream = connect(&relay, &b.id, &a.keypair).await.expect("relayed");
        let mut ch = Channel::connect(stream, &a.keypair).await.expect("end-to-end handshake");
        assert_eq!(ch.remote, b.keypair.public, "the relay cannot pose as b");
        ch.send(b"hello through the relay").await.expect("send");
        assert_eq!(ch.recv().await.expect("recv"), b"hello through the relay");
        assert_eq!(host.await.expect("host"), a.keypair.public);

        let nobody = connect(&relay, "pv-0000000000000000", &a.keypair).await;
        assert!(matches!(nobody, Err(Error::Peer(m)) if m.contains("not connected")));
        reg.abort();
        server.abort();
    }
}
