//! Agent-native REST API (`/v1`).
//!
//! Designed for autonomous agents as much as humans: every response is JSON,
//! every error has a stable machine-readable `code`, and the whole lifecycle
//! (discover → deploy → scale to zero → terminate) is four calls. Auth is a
//! bearer API key per account. The gRPC surface mirrors these routes; its
//! contract lives in `proto/peervps/v1/node.proto`.
//!
//! | method | path                           | purpose                              |
//! |--------|--------------------------------|--------------------------------------|
//! | GET    | `/v1/health`                   | liveness + hypervisor backend        |
//! | GET    | `/v1/offers`                   | filter by VRAM / price / SLA / kind  |
//! | POST   | `/v1/instances`                | deploy on an offer                   |
//! | GET    | `/v1/instances`                | list own instances                   |
//! | GET    | `/v1/instances/{id}`           | status                               |
//! | POST   | `/v1/instances/{id}/scale`     | `{"replicas":0|1}`                   |
//! | DELETE | `/v1/instances/{id}`           | terminate                            |
//! | GET    | `/v1/instances/{id}/console`   | tail of the guest serial console     |
//! | GET    | `/v1/instances/{id}/access`    | SSH endpoint, user and password      |
//! | GET    | `/v1/account`                  | balance + ledger                     |
//! | GET    | `/v1/peers`                    | this node's peer id, invite, peers   |
//! | POST   | `/v1/peers`                    | `{"address":"pv-…@host:port"}`       |
//! | POST   | `/v1/peers/{id}/approve`       | let a pending peer rent here         |
//! | DELETE | `/v1/peers/{id}`               | forget a peer                        |
//! | POST   | `/v1/webhooks/{gateway}`       | signed payment top-ups (no bearer)   |

pub mod market;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Error;
use crate::billing::payments::{self, WebhookOutcome};
use crate::node::{AccountSummary, DeployRequest, Instance, Node};
use crate::peer::{PeerInfo, PeerOverview};
use market::{Offer, OfferQuery};

pub const SIGNATURE_HEADER: &str = "peervps-signature";

/// HTTP wrapper around [`Error`].
#[derive(Debug)]
pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self.0 {
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::Capacity(_) => (StatusCode::CONFLICT, "insufficient_capacity"),
            Error::InsufficientFunds { .. } => (StatusCode::PAYMENT_REQUIRED, "insufficient_funds"),
            Error::Invalid(_) | Error::Serde(_) => (StatusCode::BAD_REQUEST, "invalid_argument"),
            Error::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Error::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "unsupported"),
            // Operator-facing detail (boot failures, bad images) is what an agent needs to retry elsewhere.
            Error::Hypervisor(_) => (StatusCode::SERVICE_UNAVAILABLE, "hypervisor_error"),
            Error::Peer(_) => (StatusCode::BAD_GATEWAY, "peer_unavailable"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self.0, "internal api error");
            "internal error".to_owned()
        } else {
            self.0.to_string()
        };
        (status, Json(json!({ "error": { "code": code, "message": message } }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Authenticated account, extracted from `Authorization: Bearer <key>`.
#[derive(Debug, Clone)]
pub struct Caller(pub String);

impl FromRequestParts<Node> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, node: &Node) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| Error::Unauthorized("missing bearer token".into()))?;
        Ok(Caller(node.authenticate(key).await?))
    }
}

pub fn router(node: Node) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/offers", get(list_offers))
        .route("/v1/instances", post(deploy).get(list_instances))
        .route("/v1/instances/{id}", get(get_instance).delete(terminate))
        .route("/v1/instances/{id}/scale", post(scale))
        .route("/v1/instances/{id}/console", get(console))
        .route("/v1/instances/{id}/access", get(access))
        .route("/v1/account", get(account))
        .route("/v1/peers", get(list_peers).post(add_peer))
        .route("/v1/peers/{id}", axum::routing::delete(remove_peer))
        .route("/v1/peers/{id}/approve", post(approve_peer))
        .route("/v1/webhooks/{gateway}", post(webhook))
        .with_state(node)
}

/// Serve the API until the future is dropped.
pub async fn serve(node: Node, addr: std::net::SocketAddr) -> crate::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "peervps api listening");
    axum::serve(listener, router(node)).await?;
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Health {
    status: &'static str,
    node_id: String,
    hypervisor: &'static str,
    version: &'static str,
}

async fn health(State(node): State<Node>) -> Json<Health> {
    Json(Health {
        status: "ok",
        node_id: node.config.node_id.clone(),
        hypervisor: node.provisioner.hypervisor().name(),
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn list_offers(State(node): State<Node>, Query(q): Query<OfferQuery>) -> Json<Vec<Offer>> {
    Json(node.offers(&q).await)
}

async fn deploy(
    State(node): State<Node>,
    Caller(who): Caller,
    Json(req): Json<DeployRequest>,
) -> Result<(StatusCode, Json<Instance>), ApiError> {
    Ok((StatusCode::CREATED, Json(node.deploy(&who, req).await?)))
}

async fn list_instances(State(node): State<Node>, Caller(who): Caller) -> Json<Vec<Instance>> {
    Json(node.instances(&who).await)
}

async fn get_instance(State(node): State<Node>, Caller(who): Caller, Path(id): Path<String>) -> ApiResult<Instance> {
    Ok(Json(node.instance(&who, &id).await?))
}

#[derive(Debug, Deserialize)]
struct ScaleRequest {
    replicas: u32,
}

async fn scale(
    State(node): State<Node>,
    Caller(who): Caller,
    Path(id): Path<String>,
    Json(req): Json<ScaleRequest>,
) -> ApiResult<Instance> {
    Ok(Json(node.scale(&who, &id, req.replicas).await?))
}

async fn terminate(State(node): State<Node>, Caller(who): Caller, Path(id): Path<String>) -> ApiResult<Instance> {
    Ok(Json(node.terminate(&who, &id).await?))
}

#[derive(Debug, Serialize)]
struct Console {
    /// `null` when the hypervisor backend does not capture a serial console.
    console: Option<String>,
}

async fn console(State(node): State<Node>, Caller(who): Caller, Path(id): Path<String>) -> ApiResult<Console> {
    Ok(Json(Console { console: node.console(&who, &id, 64 * 1024).await? }))
}

#[derive(Debug, Serialize)]
struct Access {
    /// `null` when the hypervisor backend gives guests no SSH endpoint.
    access: Option<crate::virtualization::GuestAccess>,
}

async fn access(State(node): State<Node>, Caller(who): Caller, Path(id): Path<String>) -> ApiResult<Access> {
    Ok(Json(Access { access: node.access(&who, &id).await? }))
}

async fn account(State(node): State<Node>, Caller(who): Caller) -> ApiResult<AccountSummary> {
    Ok(Json(node.account(&who).await?))
}

fn peers(node: &Node) -> Result<&crate::peer::Peers, ApiError> {
    Ok(node.peers().ok_or_else(|| Error::Unsupported("peering is off; start the node with --peer-listen".into()))?)
}

async fn list_peers(State(node): State<Node>, Caller(_): Caller) -> ApiResult<PeerOverview> {
    Ok(Json(peers(&node)?.overview().await))
}

#[derive(Debug, Deserialize)]
struct AddPeer {
    address: String,
}

async fn add_peer(State(node): State<Node>, Caller(_): Caller, Json(req): Json<AddPeer>) -> ApiResult<PeerInfo> {
    Ok(Json(peers(&node)?.add(&req.address).await?))
}

async fn approve_peer(State(node): State<Node>, Caller(_): Caller, Path(id): Path<String>) -> ApiResult<PeerInfo> {
    Ok(Json(peers(&node)?.approve(&id).await?))
}

async fn remove_peer(
    State(node): State<Node>,
    Caller(_): Caller,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    peers(&node)?.remove(&id).await?;
    Ok(Json(json!({ "removed": id })))
}

async fn webhook(
    State(node): State<Node>,
    Path(gateway): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<WebhookOutcome> {
    let sig = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::Unauthorized("missing signature header".into()))?;
    Ok(Json(payments::handle_top_up(&node.ledger, &node.webhooks, &gateway, sig, &body).await?))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::storage::now_secs;

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app.clone().oneshot(req).await.expect("response");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn agent_flow_over_http() {
        let (node, key) = Node::demo().await.expect("demo");
        let app = router(node.clone());
        let bearer = format!("Bearer {key}");

        let (s, offers) =
            call(&app, Request::get("/v1/offers?minVramMib=10000&sort=price").body(Body::empty()).expect("req")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(offers[0]["id"], "ams-4090-2");

        let (s, _) = call(&app, Request::get("/v1/account").body(Body::empty()).expect("req")).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, err) =
            call(&app, Request::get("/v1/peers").header("authorization", &bearer).body(Body::empty()).expect("req"))
                .await;
        assert_eq!((s, err["error"]["code"].as_str()), (StatusCode::NOT_IMPLEMENTED, Some("unsupported")));

        let body = json!({ "offerId": "fra-cpu-1", "spec": { "vcpus": 2, "memMib": 4096, "diskGib": 20, "image": "ubuntu-24.04" } });
        let (s, inst) = call(
            &app,
            Request::post("/v1/instances")
                .header("authorization", &bearer)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("req"),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{inst}");
        let id = inst["id"].as_str().expect("id").to_owned();

        let (s, scaled) = call(
            &app,
            Request::post(format!("/v1/instances/{id}/scale"))
                .header("authorization", &bearer)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"replicas":0}"#))
                .expect("req"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(scaled["state"], "scaledToZero");

        let payload = br#"{"id":"evt_42","account":"demo-agent","amount":1000000}"#;
        let sig = node.webhooks.sign(payload, now_secs());
        let (s, out) = call(
            &app,
            Request::post("/v1/webhooks/stub")
                .header(SIGNATURE_HEADER, sig)
                .body(Body::from(&payload[..]))
                .expect("req"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{out}");
        assert!(out["credited"]["balance"].as_i64().expect("balance") > 0);
    }
}
