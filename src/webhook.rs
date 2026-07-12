//! The `/webhook/gh` endpoint and its signature-validating data guard.
//!
//! Every delivery must carry `X-Hub-Signature-256`, the HMAC-SHA256 of the
//! raw body keyed with the shared webhook secret; anything else is rejected
//! before the payload is even parsed.
//! <https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries>

use std::sync::Arc;

use hmac::{Hmac, Mac};
use rocket::{
    Data, Request, State,
    data::{self, FromData, ToByteUnit},
    http::Status,
    serde::json,
};
use serde::de::DeserializeOwned;
use sha2::Sha256;

use crate::{App, github::payload::PullRequestWH, pipeline};

pub struct WebhookSecret(Vec<u8>);

impl WebhookSecret {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self(secret.into())
    }
}

/// Constant-time check of a `sha256=<hex>` signature header against the body.
pub fn verify_signature(secret: &[u8], header: &str, body: &[u8]) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(signature) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

#[derive(Debug)]
pub enum WebhookError {
    MissingSignature,
    BadSignature,
    TooLarge,
    Malformed,
    MissingSecret,
}

/// Data guard: reads the raw body, verifies the GitHub signature, then
/// deserializes. Payloads never reach a route without a valid signature.
pub struct GhWebhook<T>(pub T);

#[rocket::async_trait]
impl<'r, T: DeserializeOwned> FromData<'r> for GhWebhook<T> {
    type Error = WebhookError;

    async fn from_data(req: &'r Request<'_>, data: Data<'r>) -> data::Outcome<'r, Self> {
        use rocket::outcome::Outcome::{Error, Success};

        let Some(secret) = req.rocket().state::<WebhookSecret>() else {
            return Error((Status::InternalServerError, WebhookError::MissingSecret));
        };
        let Some(signature) = req.headers().get_one("X-Hub-Signature-256") else {
            tracing::warn!("webhook rejected: missing X-Hub-Signature-256");
            return Error((Status::Unauthorized, WebhookError::MissingSignature));
        };

        let limit = req.limits().get("json").unwrap_or_else(|| 1.mebibytes());
        let body = match data.open(limit).into_bytes().await {
            Ok(bytes) if bytes.is_complete() => bytes.into_inner(),
            Ok(_) => return Error((Status::PayloadTooLarge, WebhookError::TooLarge)),
            Err(_) => return Error((Status::BadRequest, WebhookError::Malformed)),
        };

        if !verify_signature(&secret.0, signature, &body) {
            tracing::warn!("webhook rejected: signature mismatch");
            return Error((Status::Unauthorized, WebhookError::BadSignature));
        }

        match json::from_slice::<T>(&body) {
            Ok(payload) => Success(GhWebhook(payload)),
            Err(e) => {
                tracing::warn!(error = %e, "webhook rejected: unparseable payload");
                Error((Status::UnprocessableEntity, WebhookError::Malformed))
            }
        }
    }
}

/// Liveness probe for reverse proxies / monitoring.
#[get("/health")]
pub fn health() -> &'static str {
    "ok"
}

/// Reviews run for minutes; GitHub times webhook deliveries out at 10s, so
/// the pipeline is spawned and the delivery acknowledged immediately.
#[post("/webhook/gh", data = "<payload>")]
pub async fn webhook_gh(
    payload: GhWebhook<PullRequestWH>,
    guardian: &State<Arc<App>>,
) -> Status {
    let guardian = guardian.inner().clone();
    tokio::spawn(pipeline::process(guardian, payload.0));
    Status::Accepted
}

#[cfg(test)]
mod tests {
    use rocket::local::asynchronous::Client;

    use super::*;
    use crate::{
        config::{Config, ReviewLimits},
        github::GhClient,
        guardian::Guardian,
        state::StateStore,
    };

    const SECRET: &[u8] = b"It's a Secret to Everybody";

    #[test]
    fn accepts_the_github_docs_test_vector() {
        // https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries
        let header = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify_signature(SECRET, header, b"Hello, World!"));
    }

    #[test]
    fn rejects_tampered_body_and_malformed_headers() {
        let header = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(!verify_signature(SECRET, header, b"Hello, World"));
        assert!(!verify_signature(SECRET, "sha256=nothex", b"Hello, World!"));
        assert!(!verify_signature(SECRET, "sha1=abc123", b"Hello, World!"));
        assert!(!verify_signature(b"wrong secret", header, b"Hello, World!"));
    }

    fn sign(body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    async fn client(dir: &tempfile::TempDir) -> Client {
        let config = Config {
            auto_merge: false,
            repos_path: dir.path().to_path_buf(),
            state_path: None,
            limits: ReviewLimits::default(),
        };
        let store = StateStore::load(config.state_path(), config.limits).unwrap();
        let guardian = Arc::new(App {
            config,
            gh: GhClient::new(octocrab::Octocrab::default()),
            guardian: Guardian::new(),
            store,
            username: None,
        });
        let rocket = rocket::build()
            .manage(guardian)
            .manage(WebhookSecret::new(SECRET))
            .mount("/", routes![webhook_gh, health]);
        Client::tracked(rocket).await.unwrap()
    }

    #[rocket::async_test]
    async fn health_responds_ok_without_auth() {
        let dir = tempfile::tempdir().unwrap();
        let client = client(&dir).await;

        let response = client.get("/health").dispatch().await;
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_string().await.as_deref(), Some("ok"));
    }

    #[rocket::async_test]
    async fn endpoint_rejects_missing_and_invalid_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let client = client(&dir).await;
        let body = r#"{"action":"labeled"}"#;

        let unsigned = client.post("/webhook/gh").body(body).dispatch().await;
        assert_eq!(unsigned.status(), Status::Unauthorized);

        let forged = client
            .post("/webhook/gh")
            .header(rocket::http::Header::new(
                "X-Hub-Signature-256",
                sign(b"other payload"),
            ))
            .body(body)
            .dispatch()
            .await;
        assert_eq!(forged.status(), Status::Unauthorized);
    }

    #[rocket::async_test]
    async fn endpoint_accepts_a_signed_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let client = client(&dir).await;
        let body = r#"{"action":"labeled"}"#;

        let response = client
            .post("/webhook/gh")
            .header(rocket::http::Header::new(
                "X-Hub-Signature-256",
                sign(body.as_bytes()),
            ))
            .body(body)
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Accepted);
    }

    #[rocket::async_test]
    async fn endpoint_rejects_signed_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let client = client(&dir).await;
        let body = "not json";

        let response = client
            .post("/webhook/gh")
            .header(rocket::http::Header::new(
                "X-Hub-Signature-256",
                sign(body.as_bytes()),
            ))
            .body(body)
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::UnprocessableEntity);
    }
}
