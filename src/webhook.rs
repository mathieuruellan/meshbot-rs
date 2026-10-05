//! The HTTP surface: a webhook that puts a message on a channel, and a read of
//! which channels it may be aimed at.
//!
//! Three properties are deliberate, and each is a decision rather than a default.
//!
//! **It carries no authentication.** Nothing here checks a token, a header or a
//! path. TLS and client-certificate verification are the reverse proxy's job, and
//! the deployment puts an mTLS-terminating proxy in front of this listener. The
//! alternative — growing a second auth path inside the process that holds the HA,
//! Komodo and Proxmox tokens — is the thing worth avoiding, not the one worth
//! having. The listener does not exist unless `MESHBOT_HTTP_ADDR` is set, so a
//! deployment that has not wired up a proxy in front gets no listener at all.
//!
//! **It can only aim at a channel this bot already listens to.** The target is
//! resolved against `bot.channels`, the same map that filters inbound messages and
//! that `verify_channels` asserts against the radio at startup. So the set of
//! channels this webhook can post on is exactly the set the bot answers on, it
//! changes only when `config.yaml` does, and it never grows a new radio channel —
//! the one thing that must never happen here, because the radio's channel table
//! belongs to mc-webui and a write to it can overwrite real channels. Nothing in this
//! module sends `SET_CHANNEL`; see the guard test in `main.rs`.
//!
//! **It never sends more than one message, and never retries.** Mesh airtime is
//! scarce and shared with everything else on the channel, so an endpoint that can be
//! made to flood the mesh is a hazard rather than a feature. The queue in front of
//! the radio is bounded and a full queue is a refusal, not a wait; a radio that does
//! not answer within `SEND_TIMEOUT` is a timeout, not a retry.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::verbs;

/// How many messages may be queued for the radio at once.
///
/// The bot handles one channel message at a time, and a script can hold that loop
/// for its whole timeout, so a queue is needed rather than a direct hand-off. It is
/// still bounded: a caller outrunning the mesh is told so instead of growing a
/// backlog that would be transmitted long after the caller stopped caring.
pub const QUEUE_DEPTH: usize = 16;

/// How long a caller waits for the radio to accept its message.
///
/// The send happens on the connection task, so a radio that had stopped answering
/// would otherwise hold the HTTP request open indefinitely. It does not: the caller
/// gets a timeout and the message is dropped. Dropping is the right failure here —
/// delivering late means the caller retries and the channel gets it twice.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest message the webhook will put on the air, in bytes.
///
/// The same limit as a reply, for the same reason: MeshCore caps a channel payload at
/// 160 bytes and `send_channel_msg` does not check, so an over-long message hangs on
/// the radio. A reply clamps, because a bot answering with half a sentence beats one
/// answering with nothing. An API caller is the other way round — it asked for a
/// specific message, so it is told the limit rather than handed a silently shortened
/// one.
const MAX_MESSAGE_BYTES: usize = verbs::MAX_REPLY_BYTES;

/// A message the caller wants on the air, handed to the connection task.
///
/// `done` is how the caller learns the outcome. This module cannot answer
/// synchronously from its own task — it does not hold the radio — so that channel is
/// the whole of the way back.
pub struct SendRequest {
    pub channel_idx: u8,
    pub text: String,
    pub done: oneshot::Sender<Result<(), String>>,
}

/// One channel the webhook may post on, as the radio knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Channel {
    pub index: u8,
    pub name: String,
}

/// Shared state for both routes: where to hand a message, and what it may be aimed at.
#[derive(Clone)]
pub struct Webhook {
    tx: mpsc::Sender<SendRequest>,
    channels: Vec<Channel>,
}

impl Webhook {
    /// Build the state from the config's listen set.
    ///
    /// Taken from `bot.channels` rather than read back from the radio on demand, so
    /// answering costs no round trips and cannot disagree with what the bot itself
    /// believes it is listening to.
    pub fn new(tx: mpsc::Sender<SendRequest>, channels: &[(u8, String)]) -> Self {
        Self {
            tx,
            channels: channels
                .iter()
                .map(|(index, name)| Channel {
                    index: *index,
                    name: name.clone(),
                })
                .collect(),
        }
    }

    /// The monitored channels, for an error that has to be actionable.
    ///
    /// Index and name together, not the index alone: a caller that addressed the
    /// channel by name and got a `404` would be told the answer in a spelling it did
    /// not use, and has to call `GET /channels` to learn what it should have said.
    fn known(&self) -> String {
        self.channels
            .iter()
            .map(|c| format!("{} {}", c.index, c.name))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Body of `POST /message`.
///
/// `deny_unknown_fields`, like every other struct in this crate that takes input:
/// a caller that misspells `message` should hear about it rather than post an empty
/// frame.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendBody {
    /// Radio channel index. Exactly one of this and `channel_name` is required. The
    /// index is the exact spelling: the radio truncates a name to 31 bytes, so a name
    /// can differ from what `GET /channels` returns for it.
    channel_idx: Option<u8>,
    /// Channel name, matched against `GET /channels` after trimming surrounding
    /// whitespace. A name the bot does not monitor is a `404`, not a `400`: the body
    /// is well formed, the target is simply not one this bot may post on.
    channel_name: Option<String>,
    message: String,
}

#[derive(Debug, Serialize)]
struct SentBody {
    channel: u8,
    name: String,
    bytes: usize,
}

/// A failure, with a body an API caller can read.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: &self.message,
            }),
        )
            .into_response()
    }
}

/// Read the listen set. Doubles as a liveness probe for the listener itself.
async fn list_channels(State(webhook): State<Webhook>) -> Json<Vec<Channel>> {
    Json(webhook.channels)
}

/// Put `message` on `channel`, and report whether the radio took it.
///
/// The send is awaited through the connection task rather than fire-and-forget, so a
/// 200 means the radio accepted the message and a 502 means it was not sent. Anything
/// looser would report success for a message that never left.
async fn send_message(
    State(webhook): State<Webhook>,
    body: Result<Json<SendBody>, JsonRejection>,
) -> Result<Json<SentBody>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("malformed body: {}", rejection.body_text()),
    })?;

    // Checked before the queue, so a refused message never occupies a slot and never
    // becomes work the connection task has to unwind.
    let found = match (body.channel_idx, body.channel_name.as_deref()) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "exactly one of channel_idx and channel_name is required".to_string(),
            });
        }
        (Some(idx), None) => webhook.channels.iter().find(|c| c.index == idx),
        // Trimmed, because a name arrives by copy-paste and the stray space is not
        // what the caller meant to ask about. The 404 below still echoes it verbatim:
        // that is what lets a caller see the space that caused the miss. Exact
        // otherwise, so this stays the same matching `verify_channels` asserts — one
        // spelling of a name, not two rules for "is this channel real" and "may I
        // post here".
        (None, Some(name)) => webhook.channels.iter().find(|c| c.name == name.trim()),
    };
    let Some(channel) = found else {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!(
                "channel {} is not monitored; GET /channels lists {}",
                body.channel_idx
                    .map(|i| i.to_string())
                    .or(body.channel_name)
                    .unwrap_or_default(),
                webhook.known()
            ),
        });
    };
    let (channel_idx, name) = (channel.index, channel.name.clone());

    let message = body.message.trim();
    if message.is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "message is empty".to_string(),
        });
    }
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!(
                "message is {} bytes; the limit is {MAX_MESSAGE_BYTES} and it is not \
                 shortened, because a shortened message is not the one that was asked for",
                message.len()
            ),
        });
    }
    let bytes = message.len();

    let (done, reported) = oneshot::channel();
    let request = SendRequest {
        channel_idx,
        text: message.to_string(),
        done,
    };

    // `try_send`, never `send`: awaiting a full queue would let one caller hold an
    // HTTP request open behind another's, and this endpoint is a broadcast channel in
    // its own right. Refusing is the honest answer.
    if let Err(err) = webhook.tx.try_send(request) {
        tracing::debug!(?err, "webhook queue refused a message");
        return Err(ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!(
                "the radio queue is full ({QUEUE_DEPTH} messages); the channel is busy, \
                 try again shortly"
            ),
        });
    }

    match tokio::time::timeout(SEND_TIMEOUT, reported).await {
        Ok(Ok(Ok(()))) => Ok(Json(SentBody {
            channel: channel_idx,
            name,
            bytes,
        })),
        // The connection task answered, and the answer was a failure.
        Ok(Ok(Err(err))) => Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: err,
        }),
        // The connection task dropped the sender, which only happens when the
        // connection ended with this request still queued.
        Ok(Err(_)) => Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: "the connection to the radio ended before the message was sent".to_string(),
        }),
        Err(_) => Err(ApiError {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: format!("the radio did not accept the message within {SEND_TIMEOUT:?}"),
        }),
    }
}

/// Bind and serve until the process ends.
///
/// A listener that will not bind is logged and the bot carries on: its job is to
/// answer messages on the mesh, and an operator who asked for a webhook and got a
/// port conflict should see an error rather than lose the mesh bot as well.
pub async fn serve(addr: SocketAddr, webhook: Webhook) {
    let app = Router::new()
        .route("/channels", get(list_channels))
        .route("/message", post(send_message))
        .with_state(webhook);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(
                %addr,
                %err,
                "webhook listener could not bind; the mesh bot is unaffected"
            );
            return;
        }
    };
    tracing::warn!(
        %addr,
        "webhook listening: it has no authentication of its own, so it must only be \
         reachable through a proxy that verifies client certificates"
    );
    if let Err(err) = axum::serve(listener, app).await {
        tracing::error!(%err, "webhook listener stopped");
    }
}

/// Fail every request still queued, and say why.
///
/// Called when a connection ends. A request left in the queue would otherwise be
/// transmitted by the *next* connection, to a caller that has already been told it
/// failed and may well have retried — and the message would arrive twice. Failing it
/// here is the outcome the caller can reason about.
pub fn fail_pending(rx: &mut mpsc::Receiver<SendRequest>, reason: &'static str) {
    let mut failed = 0usize;
    while let Ok(request) = rx.try_recv() {
        let _ = request.done.send(Err(reason.to_string()));
        failed += 1;
    }
    if failed > 0 {
        tracing::warn!(failed, "webhook messages dropped with the connection");
    }
}

/// The address to listen on, or `None` when the webhook is not configured.
///
/// One variable rather than a host and a port: an address is one thing to parse and
/// one thing to get wrong, and there is no case here for binding an interface and a
/// port separately.
pub fn listen_addr() -> anyhow::Result<Option<SocketAddr>> {
    let Ok(raw) = std::env::var("MESHBOT_HTTP_ADDR") else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse() {
        Ok(addr) => Ok(Some(addr)),
        Err(err) => Err(anyhow::anyhow!(
            "MESHBOT_HTTP_ADDR must be host:port, got {raw:?}: {err}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// A state wired to a queue this test owns, so nothing reaches a radio.
    fn harness() -> (Webhook, mpsc::Receiver<SendRequest>) {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let webhook = Webhook::new(tx, &[(0, "#bot".to_string()), (3, "#admin".to_string())]);
        (webhook, rx)
    }

    /// A body to hand the handler, bypassing axum's own JSON extraction: what is
    /// under test here is the policy, and `deny_unknown_fields` is serde's.
    fn body(channel: u8, message: &str) -> Result<Json<SendBody>, JsonRejection> {
        Ok(Json(SendBody {
            channel_idx: Some(channel),
            channel_name: None,
            message: message.to_string(),
        }))
    }

    fn named(
        channel_name: Option<&str>,
        channel_idx: Option<u8>,
    ) -> Result<Json<SendBody>, JsonRejection> {
        Ok(Json(SendBody {
            channel_idx,
            channel_name: channel_name.map(str::to_string),
            message: "hello".to_string(),
        }))
    }

    /// The rendered body of an error response, so a test can assert the message a
    /// caller would actually read.
    async fn error_text(error: ApiError) -> String {
        let response = error.into_response();
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body is readable");
        String::from_utf8(bytes.to_vec()).expect("body is utf-8")
    }

    #[tokio::test]
    async fn lists_exactly_the_monitored_channels() {
        let (webhook, _rx) = harness();
        let Json(channels) = list_channels(State(webhook)).await;
        assert_eq!(
            channels,
            vec![
                Channel {
                    index: 0,
                    name: "#bot".to_string(),
                },
                Channel {
                    index: 3,
                    name: "#admin".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_request_is_queued_and_reported_as_sent() {
        let (webhook, mut rx) = harness();

        let caller = tokio::spawn(send_message(
            State(webhook),
            body(3, "  porte du garage ouverte  "),
        ));

        // The connection task's side of the same request.
        let request = rx.recv().await.expect("a request is queued");
        assert_eq!(request.channel_idx, 3);
        // Trimmed, not reworded: what goes on the air is what the caller sent minus
        // the whitespace.
        assert_eq!(request.text, "porte du garage ouverte");
        request.done.send(Ok(())).expect("caller is still waiting");

        let response = caller.await.expect("handler does not panic");
        assert_eq!(
            response.expect("accepted").into_response().status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_channel_the_bot_does_not_listen_to_is_refused() {
        let (webhook, mut rx) = harness();

        // Channel 1 exists on the radio and belongs to the phone app and the family.
        // The webhook must not become a way to post on it.
        let error = send_message(State(webhook), body(1, "hello"))
            .await
            .expect_err("channel 1 is not monitored");

        assert_eq!(error.status, StatusCode::NOT_FOUND);
        // The error has to say what *is* allowed, or the caller has to guess — in the
        // spelling they can use, so both the index and the name.
        assert!(error_text(error).await.contains("0 #bot, 3 #admin"));
        assert!(
            rx.try_recv().is_err(),
            "a refused channel must not be queued"
        );
    }

    #[tokio::test]
    async fn a_channel_can_be_addressed_by_name() {
        let (webhook, mut rx) = harness();
        let caller = tokio::spawn(send_message(State(webhook), named(Some("#admin"), None)));
        let request = rx.recv().await.expect("a request is queued");
        assert_eq!(request.channel_idx, 3);
        request.done.send(Ok(())).expect("caller is still waiting");
        assert_eq!(
            caller
                .await
                .expect("handler does not panic")
                .expect("accepted")
                .into_response()
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn an_unknown_name_is_a_404() {
        let (webhook, mut rx) = harness();
        let error = send_message(State(webhook), named(Some("#nope"), None))
            .await
            .expect_err("unknown name");
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert!(error_text(error).await.contains("0 #bot, 3 #admin"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_channel_that_exists_but_is_not_monitored_is_refused_by_name() {
        let (webhook, mut rx) = harness();

        // "#family" is a real channel on the radio and belongs to the phone app and
        // the family. Addressing it by name must not become a way to post on it — the
        // index form of this is refused above, and the name form is the new path.
        let error = send_message(State(webhook), named(Some("#family"), None))
            .await
            .expect_err("#family is not monitored");

        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert!(
            rx.try_recv().is_err(),
            "a refused channel must not be queued"
        );
    }

    #[tokio::test]
    async fn a_name_is_trimmed_before_it_is_matched() {
        let (webhook, mut rx) = harness();

        // A name arrives by copy-paste; the stray spaces are not the question.
        let caller = tokio::spawn(send_message(
            State(webhook),
            named(Some("  #admin  "), None),
        ));
        let request = rx.recv().await.expect("a request is queued");
        assert_eq!(request.channel_idx, 3);
        request.done.send(Ok(())).expect("caller is still waiting");
        assert_eq!(
            caller
                .await
                .expect("handler does not panic")
                .expect("accepted")
                .into_response()
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_name_that_misses_is_echoed_verbatim() {
        let (webhook, _rx) = harness();

        // The trimmed name is what gets matched, but the 404 quotes what was sent —
        // otherwise a caller whose stray whitespace caused the miss is told
        // `#family is not monitored` when they never asked about `#family`.
        let error = send_message(State(webhook), named(Some(" #family "), None))
            .await
            .expect_err("#family is not monitored");
        assert!(
            error_text(error)
                .await
                .contains("channel  #family  is not monitored")
        );
    }

    #[tokio::test]
    async fn exactly_one_of_idx_and_name_is_required() {
        for (name, idx) in [(None, None), (Some("#bot"), Some(0))] {
            let (webhook, _rx) = harness();
            let error = send_message(State(webhook), named(name, idx))
                .await
                .expect_err("ambiguous or missing channel");
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn an_over_long_message_is_refused_rather_than_shortened() {
        let (webhook, mut rx) = harness();
        let message = "a".repeat(MAX_MESSAGE_BYTES + 1);

        let error = send_message(State(webhook), body(0, &message))
            .await
            .expect_err("over the limit");

        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            rx.try_recv().is_err(),
            "a refused message must not be queued"
        );
    }

    #[tokio::test]
    async fn the_limit_is_measured_in_bytes_not_characters() {
        // `é` is two bytes, so 76 of them is 152: over the limit while well under
        // 150 characters. A character count would wave this through.
        let (webhook, _rx) = harness();
        let message = "é".repeat(MAX_MESSAGE_BYTES / 2 + 1);

        let error = send_message(State(webhook), body(0, &message))
            .await
            .expect_err("over the byte limit");

        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_message_at_exactly_the_limit_is_accepted() {
        let (webhook, mut rx) = harness();
        let message = "a".repeat(MAX_MESSAGE_BYTES);

        let caller = tokio::spawn(send_message(State(webhook), body(0, &message)));
        let request = rx.recv().await.expect("a request is queued");
        assert_eq!(request.text.len(), MAX_MESSAGE_BYTES);
        request.done.send(Ok(())).expect("caller is still waiting");

        let response = caller.await.expect("handler does not panic");
        assert_eq!(
            response.expect("accepted").into_response().status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_whitespace_only_message_is_refused() {
        let (webhook, mut rx) = harness();

        let error = send_message(State(webhook), body(0, " \t "))
            .await
            .expect_err("nothing to send");

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_full_queue_is_refused_rather_than_queued() {
        let (webhook, _rx) = harness();
        for _ in 0..QUEUE_DEPTH {
            webhook
                .tx
                .clone()
                .try_send(SendRequest {
                    channel_idx: 0,
                    text: "already queued".to_string(),
                    done: oneshot::channel().0,
                })
                .expect("the queue has room");
        }

        let error = send_message(State(webhook), body(0, "one too many"))
            .await
            .expect_err("the queue is full");

        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_radio_failure_is_reported_as_a_bad_gateway() {
        let (webhook, mut rx) = harness();

        let caller = tokio::spawn(send_message(State(webhook), body(0, "hello")));
        let request = rx.recv().await.expect("a request is queued");
        request
            .done
            .send(Err("radio not connected".to_string()))
            .expect("caller is still waiting");

        let error = caller
            .await
            .expect("handler does not panic")
            .expect_err("the radio refused it");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert!(error_text(error).await.contains("radio not connected"));
    }

    #[tokio::test]
    async fn a_wedged_radio_times_out_instead_of_hanging_the_caller() {
        let (webhook, mut rx) = harness();

        // The request is taken and never answered, which is what a radio that has
        // stopped answering looks like from this side.
        let caller = tokio::spawn(send_message(State(webhook), body(0, "hello")));
        let _request = rx.recv().await.expect("a request is queued");

        let error = tokio::time::timeout(SEND_TIMEOUT * 3, caller)
            .await
            .expect("the handler gives up rather than hanging the caller")
            .expect("handler does not panic")
            .expect_err("never answered");
        assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn pending_requests_are_failed_when_the_connection_ends() {
        let (tx, mut rx) = mpsc::channel(QUEUE_DEPTH);
        let (done, reported) = oneshot::channel();
        tx.try_send(SendRequest {
            channel_idx: 0,
            text: "queued".to_string(),
            done,
        })
        .expect("room in the queue");

        fail_pending(&mut rx, "the connection to the radio ended");

        let outcome = reported.await.expect("fail_pending answers every request");
        assert_eq!(
            outcome.expect_err("it was never sent"),
            "the connection to the radio ended"
        );
    }

    #[tokio::test]
    async fn failing_pending_requests_is_a_no_op_when_nothing_is_queued() {
        let (_tx, mut rx) = mpsc::channel(QUEUE_DEPTH);
        fail_pending(&mut rx, "the connection to the radio ended");
    }
}
