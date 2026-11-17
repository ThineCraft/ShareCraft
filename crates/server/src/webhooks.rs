//! `POST /hooks/github/{library}`: GitHub's webhook deliveries for a connected library.
//!
//! A delivery is taken only when its `X-Hub-Signature-256` is the HMAC-SHA256 of the raw body
//! under the library's webhook secret (the secret's hexadecimal text, as GitHub was given it),
//! compared in constant time. At most [`MAX_IN_FLIGHT`] bodies are handled at once; excess is
//! refused before its body is read. A taken body (at most [`MAX_BODY`]) is read in full before
//! the check. Nothing in a delivery is trusted but the branch name of a push, which only decides
//! whether the library is woken: the sync itself reads the branch from GitHub.
//!
//! Answers: `200` for a `ping`; `202` for a `push` to the connected branch, which wakes the sync
//! worker; `204` for any other push or event; `401` for a missing or wrong signature; `404` for
//! an address that names no connected library, or one with no webhook secret. No answer says
//! more, and no secret, signature or body reaches a log line, an event or an error. `401` and
//! `404` answers count towards the webhook throttle (per address and library for existing
//! libraries, address-only for unknown ones; see `lib.rs`), so guessing is refused with `429`.

use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use uuid::Uuid;

use crate::AppState;
use crate::secrets::Sealed;
use crate::source_sync::webhook_context;

/// The largest delivery taken: 5 MiB (GitHub caps its payloads at 25 MiB, but a push of a
/// documents repository is far smaller, and only its branch name is read).
pub(crate) const MAX_BODY: usize = 5 << 20;
/// The most webhook bodies read or handled at once across this server.
pub(crate) const MAX_IN_FLIGHT: usize = 8;

/// A body read only after taking a global webhook turn. The permit stays held through the
/// handler, and an excess request is refused before its body is polled.
pub(crate) struct Delivery {
    body: Bytes,
    _turn: tokio::sync::OwnedSemaphorePermit,
}

impl FromRequest<AppState> for Delivery {
    type Rejection = Response;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let Some(turn) = state.webhook_turn() else {
            return Err(bare(StatusCode::TOO_MANY_REQUESTS));
        };
        let body = Bytes::from_request(request, state)
            .await
            .map_err(IntoResponse::into_response)?;
        Ok(Delivery { body, _turn: turn })
    }
}

/// The parts of a push delivery that are read.
#[derive(Deserialize)]
struct Push {
    /// `refs/heads/<branch>` for a branch.
    #[serde(rename = "ref")]
    reference: String,
}

/// A refusal that says nothing more than its status.
fn bare(status: StatusCode) -> Response {
    status.into_response()
}

/// The handler (see the module documentation).
pub(crate) async fn github(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Delivery { body, _turn }: Delivery,
) -> Response {
    let response = handle(&state, &id, &headers, &body).await;
    drop(_turn);
    response
}

async fn handle(state: &AppState, id: &str, headers: &HeaderMap, body: &Bytes) -> Response {
    let Ok(uuid) = Uuid::parse_str(id) else {
        return bare(StatusCode::NOT_FOUND);
    };
    let library = NodeId(uuid);
    let config = match state.db().source_of(library).await {
        Ok(Some((config, _))) => config,
        Ok(None) => return bare(StatusCode::NOT_FOUND),
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return bare(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let Some(secret_id) = config.webhook_secret else {
        return bare(StatusCode::NOT_FOUND);
    };
    let Some(signature) = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("sha256="))
        .and_then(from_hex)
    else {
        return bare(StatusCode::UNAUTHORIZED);
    };
    let row = match state.db().get_secret(secret_id).await {
        Ok(Some(row)) => row,
        // Removed meanwhile: as if none were set.
        Ok(None) => return bare(StatusCode::NOT_FOUND),
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return bare(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let sealed = Sealed {
        key_id: row.key_id,
        nonce: row.nonce,
        ciphertext: row.ciphertext,
    };
    let verified = {
        let Ok(key) = state.secrets().unseal(&sealed, &webhook_context(library)) else {
            eprintln!(
                "sharecraft-server: the webhook secret of library {} could not be opened",
                library.0
            );
            return bare(StatusCode::NOT_FOUND);
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&key) else {
            return bare(StatusCode::NOT_FOUND);
        };
        mac.update(body);
        // Constant time. The opened secret goes when this block ends.
        mac.verify_slice(&signature).is_ok()
    };
    if !verified {
        return bare(StatusCode::UNAUTHORIZED);
    }
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    match event {
        "ping" => bare(StatusCode::OK),
        "push" => {
            let wanted = format!("refs/heads/{}", config.branch);
            let ours = serde_json::from_slice::<Push>(body).is_ok_and(|p| p.reference == wanted);
            if ours {
                state.sources().wake(library);
                bare(StatusCode::ACCEPTED)
            } else {
                bare(StatusCode::NO_CONTENT)
            }
        }
        _ => bare(StatusCode::NO_CONTENT),
    }
}

/// The bytes of lower-case hexadecimal `text` of a SHA-256 (64 digits); `None` otherwise.
fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[high, low]| Some((digit(*high)? << 4) | digit(*low)?))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn hex_signatures() {
        let good = "00ff".repeat(16);
        assert_eq!(from_hex(&good).unwrap()[..2], [0x00, 0xff]);
        assert_eq!(from_hex(&good).unwrap().len(), 32);
        assert!(from_hex(&good.to_uppercase()).is_none());
        assert!(from_hex(&good[..62]).is_none());
        assert!(from_hex(&format!("{}zz", &good[..62])).is_none());
        assert!(from_hex("").is_none());
    }

    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        let expected =
            from_hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843").unwrap();
        assert!(mac.verify_slice(&expected).is_ok());
    }
}
