//! This module exposes the sate of the server via an HTTP interface. As such it primary concerns
//! are modding success and error states to HTTP status codes. Defining in which formats to
//! deserialize paramaters and serialize respones, or deciding on which HTTP methods to map the
//! functions.

use crate::{error::ThrottleError, leases::PeerId, state::State};
use actix_web::{
    delete, get,
    http::StatusCode,
    post, put,
    web::{Data, Json, Path, Query},
    HttpResponse, ResponseError,
};
use log::{debug, warn};
use serde::Deserialize;
use std::{collections::HashMap, time::Duration};

impl ResponseError for ThrottleError {
    fn status_code(&self) -> StatusCode {
        match self {
            ThrottleError::UnknownPeer => StatusCode::BAD_REQUEST,
            ThrottleError::UnknownSemaphore => StatusCode::BAD_REQUEST,
            ThrottleError::ForeverPending { .. } => StatusCode::CONFLICT,
            ThrottleError::Deadlock => StatusCode::CONFLICT,
            ThrottleError::NotImplemented => StatusCode::NOT_IMPLEMENTED,
        }
    }
}

type Leases = HashMap<String, u32>;

/// Strict alias around `Duration`. Yet it serializes from a human readable representation.
#[derive(Deserialize)]
struct HumanDuration(#[serde(with = "humantime_serde")] Duration);

/// Used as a query parameter in requests. E.g. `?expires_in=5m`.
#[derive(Deserialize)]
struct ExpiresIn {
    #[serde(with = "humantime_serde")]
    expires_in: Duration,
}

/// Parameters for heartbeat to a lease
#[derive(Deserialize)]
pub struct ActiveLeases {
    active: Leases,
    /// Duration in seconds. After the specified time has passed the lease may be freed by litter
    /// collection.
    #[serde(with = "humantime_serde")]
    expires_in: Duration,
}

impl ActiveLeases {
    fn active(&self) -> Option<(&str, u32)> {
        self.active
            .iter()
            .next()
            .map(|(sem, &amount)| (sem.as_str(), amount))
    }
}

/// Create a new peer with no acquired locks.
///
/// Returns id of the new peer
#[post("/new_peer")]
async fn new_peer(body: Json<ExpiresIn>, state: Data<State>) -> Json<PeerId> {
    Json(state.new_peer(body.expires_in))
}

#[delete("/peers/{id}")]
async fn release(path: Path<PeerId>, state: Data<State>) -> HttpResponse {
    if state.release(*path) {
        HttpResponse::Ok().json("Peer released")
    } else {
        // Post condition of lease not being there is satisfied, let's make this request 200 still.
        HttpResponse::Ok().json("Peer not found")
    }
}

/// Acquire a lock to a Semaphore. Does not block.
#[put("/peer/{id}/{semaphore}")]
async fn acquire(
    path: Path<(PeerId, String)>,
    query: Query<ExpiresIn>,
    body: Json<u32>,
    state: Data<State>,
) -> HttpResponse {
    let amount = body.0;
    let peer_id = path.0;
    let semaphore = &path.1;
    match state.acquire(peer_id, semaphore, amount, query.expires_in) {
        Ok(true) => HttpResponse::Ok().json(peer_id),
        Ok(false) => HttpResponse::Accepted().json(peer_id),
        Err(error) => HttpResponse::from_error(error.into()),
    }
}

#[derive(Deserialize)]
struct MaxTimeout {
    timeout_ms: Option<u64>,
}

/// Waits for a ticket to be promoted to a lease
///
/// This function is supposed to be called repeatedly from client side, until the leases are
/// active. It also updates the expiration timeout to prevent the litter collection from
/// removing the peer while it is pending. Having repeated short lived requests is preferable
/// over one long running, as many proxies, firewalls, and Gateways might kill them.
///
/// ## Return
///
/// Returns `true` if leases are active.
#[post("/peers/{id}/block_until_acquired")]
async fn block_until_acquired(
    path: Path<PeerId>,
    query: Query<MaxTimeout>,
    body: Json<ExpiresIn>,
    state: Data<State>,
) -> Result<Json<bool>, ThrottleError> {
    let lease_id = *path;
    let unblock_after = Duration::from_millis(query.timeout_ms.unwrap_or(0));
    debug!(
        "Lease {} is waiting for admission with timeout {:?}",
        lease_id, unblock_after
    );
    let peer_id = *path;
    let acquired_in_time = state
        .block_until_acquired(peer_id, body.expires_in, unblock_after)
        .await?;
    Ok(Json(acquired_in_time))
}

#[derive(Deserialize)]
pub struct Restore {
    #[serde(with = "humantime_serde")]
    expires_in: Duration,
    peer_id: PeerId,
    pending: Leases,
}

#[post("/restore")]
pub async fn restore(body: Json<Restore>, state: Data<State>) -> Result<Json<bool>, ThrottleError> {
    let (semaphore, &count) = body
        .pending
        .iter()
        .next()
        .ok_or(ThrottleError::NotImplemented)?;
    state
        .restore_pending(body.peer_id, body.expires_in, semaphore, count)
        .map(Json)
}

/// Query parameters for getting remaining semaphore count
#[derive(Deserialize)]
struct Remainder {
    semaphore: String,
}

/// Get the remainder of a semaphore
#[get("/remainder")]
async fn remainder(
    query: Query<Remainder>,
    state: Data<State>,
) -> Result<Json<i64>, ThrottleError> {
    state.remainder(&query.semaphore).map(Json)
}

/// Returns wether all the locks of the peer have been acquired. This route will not block, but
/// return immediatly.
#[get("/peers/{id}/is_acquired")]
async fn is_acquired(path: Path<PeerId>, state: Data<State>) -> Result<Json<bool>, ThrottleError> {
    state.is_acquired(*path).map(Json)
}

/// Manually remove all expired semapahores. Usefull for testing
#[post("/remove_expired")]
async fn remove_expired(state: Data<State>) -> Json<usize> {
    debug!("Remove expired triggered");
    Json(state.remove_expired())
}

#[put("/peers/{id}")]
async fn put_peer(
    path: Path<PeerId>,
    body: Json<ActiveLeases>,
    state: Data<State>,
) -> Result<&'static str, ThrottleError> {
    let lease_id = *path;
    if let Some((semaphore, amount)) = body.active() {
        debug!("Received heartbeat for {}", lease_id);
        state.heartbeat_for_active_peer(lease_id, semaphore, amount, body.expires_in)?;
    } else {
        warn!("Empty heartbeat (no active leases) for {}", lease_id);
    }
    Ok("Ok")
}
