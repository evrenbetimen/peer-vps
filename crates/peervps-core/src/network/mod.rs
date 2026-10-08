//! Compressed, encrypted P2P overlay network (ZeroTier-style).
//!
//! Data path for a packet leaving a guest towards a client:
//!
//! ```text
//! guest virtio-net ─▶ host TUN (10.147.0.0/16) ─▶ RoutingTable::route_packet
//!   ─▶ TunnelCodec::seal (zstd → ChaCha20-Poly1305) ─▶ UDP socket (hole-punched)
//! ```
//!
//! The session keys come from a Noise IK handshake ([`noise`]); the UDP path
//! comes from STUN discovery ([`stun`]) plus simultaneous-open punching
//! ([`holepunch`]).

pub mod codec;
pub mod holepunch;
pub mod noise;
pub mod routing;
pub mod stun;
#[cfg(target_os = "linux")]
pub mod tun_device;

pub use codec::{CodecConfig, TunnelCodec};
pub use routing::{Route, RoutingTable};

/// Default overlay subnet for guest virtual IPs.
pub const OVERLAY_NET: std::net::Ipv4Addr = std::net::Ipv4Addr::new(10, 147, 0, 0);
pub const OVERLAY_PREFIX: u8 = 16;
/// UDP payload budget: 1500 Ethernet − 20 IP − 8 UDP − tunnel header and tag.
pub const TUNNEL_MTU: u16 = 1500 - 20 - 8 - (codec::HEADER_LEN + codec::TAG_LEN) as u16;
