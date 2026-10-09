//! Messages nodes exchange over a [`super::channel::Channel`]: one JSON request,
//! one JSON response, then the connection closes (or, after `Forward`, carries
//! a guest port's bytes).

use serde::{Deserialize, Serialize};

use crate::api::market::Offer;
use crate::node::Instance;
use crate::virtualization::{GuestAccess, VmSpec};
use crate::{Error, Result};

/// Which port of a guest a renter wants carried to its own machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GuestPort {
    Ssh,
    Rdp,
    Display,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Request {
    /// Introduce ourselves; `listen_port` (or `relay`, the relay we can be
    /// reached through) lets the host dial back once it approves us, so
    /// renting works in both directions.
    Hello {
        listen_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay: Option<String>,
    },
    Deploy {
        offer_id: String,
        spec: VmSpec,
    },
    Get {
        id: String,
    },
    Scale {
        id: String,
        replicas: u32,
    },
    Terminate {
        id: String,
    },
    Console {
        id: String,
        max_bytes: usize,
    },
    Access {
        id: String,
    },
    /// After an `Ok` answer the channel carries this guest port's bytes.
    Forward {
        id: String,
        port: GuestPort,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Response {
    Welcome { id: String, offers: Vec<Offer> },
    Instance { instance: Instance },
    Console { console: Option<String> },
    Access { access: Option<GuestAccess> },
    Ok,
    Error { code: String, message: String },
}

impl Response {
    pub fn from_error(e: &Error) -> Self {
        let code = match e {
            Error::NotFound(_) => "not_found",
            Error::Capacity(_) => "insufficient_capacity",
            Error::InsufficientFunds { .. } => "insufficient_funds",
            Error::Invalid(_) | Error::Serde(_) => "invalid_argument",
            Error::Unauthorized(_) => "unauthorized",
            Error::Unsupported(_) => "unsupported",
            Error::Hypervisor(_) => "hypervisor_error",
            _ => "internal",
        };
        let message = match e {
            // The renter re-wraps it in the same variant, so send the text without its prefix.
            Error::NotFound(m)
            | Error::Capacity(m)
            | Error::Invalid(m)
            | Error::Unauthorized(m)
            | Error::Unsupported(m)
            | Error::Hypervisor(m)
            | Error::Peer(m) => m.clone(),
            // Keep the host's internals (paths, SQL) on the host.
            Error::Db(_) | Error::Io(_) | Error::Join(_) | Error::Crypto(_) => "internal error on the host".to_owned(),
            _ => e.to_string(),
        };
        Self::Error { code: code.to_owned(), message }
    }

    /// Turn an `Error` answer back into an [`Error`], prefixed with the host.
    pub fn into_result(self, host: &str) -> Result<Self> {
        let Self::Error { code, message } = self else { return Ok(self) };
        let message = format!("{host}: {message}");
        Err(match code.as_str() {
            "not_found" => Error::NotFound(message),
            "insufficient_capacity" => Error::Capacity(message),
            "invalid_argument" => Error::Invalid(message),
            "unauthorized" => Error::Unauthorized(message),
            "unsupported" => Error::Unsupported(message),
            "hypervisor_error" => Error::Hypervisor(message),
            // The host's ledger, not ours: a plain message reads better than our numbers.
            _ => Error::Peer(message),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_tagged_camel_case() {
        let r = Request::Forward { id: "inst-1".into(), port: GuestPort::Rdp };
        assert_eq!(serde_json::to_string(&r).expect("json"), r#"{"op":"forward","id":"inst-1","port":"rdp"}"#);
        let r = Request::Hello { listen_port: Some(7071), relay: None };
        assert_eq!(serde_json::to_string(&r).expect("json"), r#"{"op":"hello","listenPort":7071}"#);
        let e = Response::from_error(&Error::NotFound("instance x".into()));
        assert!(matches!(e.clone().into_result("pv-1"), Err(Error::NotFound(m)) if m == "pv-1: instance x"));
        let io = Response::from_error(&Error::Io(std::io::Error::other("/secret/path")));
        assert!(!serde_json::to_string(&io).expect("json").contains("secret"));
    }
}
