//! meshbot-rs — MeshCore channel rule/action bot.
//!
//! Connects to the radio over TCP, verifies the channel map from `config.yaml`,
//! then streams channel messages. A message is parsed into a command context,
//! resolved against the verb table, and either answered or run as the script the
//! config names for it.
//!
//! The vocabulary is data, not code: which channels are listened to, which verbs
//! exist and which script each one runs all come from `config.yaml`, validated
//! at startup. See `config`.

mod config;
mod latch;
mod parse;
mod script;
mod verbs;

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use meshcore_rs::{EventPayload, MeshCore, PayloadType};
use parse::ParseError;

/// How long to wait between read-only liveness probes of the radio.
///
/// The proxy keeps a client's TCP socket open when the radio disconnects — it
/// flips an internal flag and drops commands — so there is no disconnect to
/// react to. Liveness has to be polled. The probe is a `GET_DEVICE_TIME`, which
/// is read-only and touches no table this bot shares with mc-webui.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long to wait before redialling after a connection ends.
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    dotenvy::dotenv().ok();

    let config_path =
        std::env::var("MESHBOT_CONFIG").unwrap_or_else(|_| config::DEFAULT_CONFIG_PATH.to_string());
    let loaded = config::load(std::path::Path::new(&config_path))?;
    tracing::info!(
        config = %config_path,
        channels = loaded.channels.len(),
        verbs = loaded.table.names().join(" "),
        script_dir = %loaded.script_dir.display(),
        "config loaded"
    );

    let host = std::env::var("MESHCORE_HOST").unwrap_or_else(|_| "proxy".to_string());
    let port: u16 = std::env::var("MESHCORE_PORT")
        .unwrap_or_else(|_| "5000".to_string())
        .parse()
        .context("MESHCORE_PORT must be a number")?;

    tracing::info!(version = env!("CARGO_PKG_VERSION"), host = %host, port, "starting");

    // The connection is not expected to last: the radio can be power-cycled and
    // the proxy restarted under it. Every connection re-runs the clock sync and
    // the channel assertion, so recovery is a reconnect, not a container restart.
    loop {
        match run_connection(&host, port, &loaded).await {
            Ended::Reconnect(err) => {
                tracing::warn!(%err, delay = ?RECONNECT_DELAY, "connection lost; reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Ended::Fatal(err) => return Err(err),
        }
    }
}

/// Why a connection ended.
///
/// The distinction matters: a transport or radio failure is worth redialling,
/// but a config/radio-layout disagreement is not — no amount of reconnecting
/// changes what `config.yaml` asserts the radio should hold.
enum Ended {
    /// The transport or the radio failed. Reconnect.
    Reconnect(anyhow::Error),
    /// The config and the radio disagree. Stop, as the startup assertion always has.
    Fatal(anyhow::Error),
}

/// One connection, from dial to the moment it is no longer usable.
///
/// Returns rather than loops so the reconnect policy lives in one place in
/// [`main`]. The latch is created here, per connection, so a reconnect disarms
/// any pending confirmation: a two-step action must be re-issued against the
/// connection that armed it.
async fn run_connection(host: &str, port: u16, loaded: &config::Loaded) -> Ended {
    let meshcore = match MeshCore::tcp(host, port).await {
        Ok(meshcore) => meshcore,
        Err(err) => return Ended::Reconnect(err.into()),
    };

    let info = match meshcore.commands().lock().await.send_appstart().await {
        Ok(info) => info,
        Err(err) => {
            disconnect(&meshcore).await;
            return Ended::Reconnect(err.into());
        }
    };
    tracing::info!(name = %info.name, "app started");

    // Time first, so a channel message the radio sends after connecting
    // comes with a clock that is at least close. The function is best-effort:
    // a failure to query or set the clock does not stop the bot.
    set_radio_clock(&meshcore).await;

    if let Err(err) = verify_channels(&meshcore, &loaded.channels).await {
        disconnect(&meshcore).await;
        return Ended::Fatal(err);
    }

    // Best effort: the hop hashes in the RF log resolve to names only when the
    // repeater is a contact. A radio that will not hand over its contact list
    // must not stop the bot; unresolved hops fall back to their hex id.
    match meshcore.ensure_contacts().await {
        Ok(()) => tracing::debug!("contact cache loaded for path name resolution"),
        Err(err) => tracing::warn!(%err, "could not load contacts; hop ids stay hex"),
    }

    // Returns unit, not a Result — there is nothing to await that can fail.
    meshcore.start_auto_message_fetching().await;
    tracing::info!("listening for channel messages");

    let script_dir = &loaded.script_dir;
    let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
    // The radio pushes the RF log (`LOG_DATA`) for every packet it receives,
    // immediately before the fetched message that carries the text. The header
    // of that log is the only place the hop path appears: `ChannelMessage`
    // itself carries `path_len` but not the hashes. So the most recent channel
    // packet's path is held here and attached to the next message that matches
    // it, and dropped otherwise rather than guessed at.
    let mut pending_path: Option<PendingPath> = None;

    let mut stream = meshcore.event_stream();

    // `interval` fires immediately on its first tick; consume it so the first
    // probe is one interval out rather than at connect time. `Delay` keeps a
    // long-running action from making the next probes fire in a burst.
    let mut probe = tokio::time::interval(PROBE_INTERVAL);
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    probe.tick().await;

    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else {
                    // The stream ends only when its sender is gone, which is the
                    // transport being unusable.
                    disconnect(&meshcore).await;
                    return Ended::Reconnect(anyhow::anyhow!("channel event stream ended"));
                };
                match event.payload {
                    EventPayload::LogData(log) => {
                        let Some(header) = log.header else {
                            continue;
                        };
                        if header.payload_type != PayloadType::GroupText {
                            continue;
                        }
                        pending_path = Some(PendingPath {
                            path_len: header.path_len,
                            hashes: split_path(&header.path, header.path_len, header.path_hash_size),
                            at: Instant::now(),
                        });
                    }
                    EventPayload::ChannelMessage(msg) => {
                        // `msg.path_len` is the packed wire byte, not a hop count.
                        let hops = hop_count(msg.path_len);

                        // Consume the logged path for whatever message comes next,
                        // monitored or not, so a packet on a channel this bot ignores
                        // cannot leave its route to be attached to a later reply.
                        let correlated = pending_path
                            .take()
                            .filter(|p| correlate(p, hops, Instant::now()));

                        let Some((_, name)) = loaded
                            .channels
                            .iter()
                            .find(|(idx, _)| *idx == msg.channel_idx)
                        else {
                            // Not in the listen set. The channel map is both the filter and
                            // the startup assertion, so a message from anywhere else is
                            // dropped before it is even parsed.
                            tracing::debug!(channel_idx = msg.channel_idx, "channel not monitored");
                            continue;
                        };
                        tracing::info!(
                            channel_idx = msg.channel_idx,
                            message_id = msg.message_id(),
                            hops,
                            text = %msg.text,
                            "channel message"
                        );

                        let resolved = match &correlated {
                            Some(p) if !p.hashes.is_empty() => {
                                let mut names = Vec::with_capacity(p.hashes.len());
                                for hash in &p.hashes {
                                    names.push(
                                        meshcore
                                            .get_contact_by_prefix(hash)
                                            .await
                                            .map(|c| c.adv_name),
                                    );
                                }
                                render_repeaters(&p.hashes, &names)
                            }
                            _ => String::new(),
                        };
                        let repeaters = chain(correlated.as_ref(), &resolved);

                        let meta = parse::MessageMeta {
                            path_len: hops,
                            snr: msg.snr,
                            sender_timestamp: msg.sender_timestamp,
                            repeaters,
                            now: now_secs(),
                        };

                        // Sequential, deliberately. One message is resolved, executed and
                        // answered before the next is read, so two actions cannot interleave
                        // their scripts or their replies.
                        handle_message(
                            &meshcore,
                            loaded,
                            &mut latch,
                            script_dir,
                            &msg.text,
                            name,
                            msg.channel_idx,
                            &meta,
                        )
                        .await;
                    }
                    _ => {}
                }
            }
            _ = probe.tick() => {
                if let Err(err) = radio_is_alive(&meshcore).await {
                    // The proxy kept the socket open and dropped the command, so
                    // this is the only signal that the radio is gone. Redialling is
                    // what re-runs set_radio_clock().
                    disconnect(&meshcore).await;
                    return Ended::Reconnect(err);
                }
                tracing::debug!("radio liveness probe ok");
            }
        }
    }
}

/// Ask the radio for its clock as a liveness check.
///
/// Read-only on purpose: `GET_DEVICE_TIME` touches no table this bot shares
/// with mc-webui, and a failure here means the proxy is dropping commands
/// because the radio is not connected.
async fn radio_is_alive(meshcore: &MeshCore) -> Result<()> {
    let commands = meshcore.commands().lock().await;
    commands
        .get_time()
        .await
        .context("radio did not answer GET_DEVICE_TIME")?;
    Ok(())
}

/// Drop the background tasks before the client goes away.
///
/// `MeshCore` has no `Drop` that aborts its read/write tasks, so a client that
/// is replaced without this leaves them running until the process exits.
async fn disconnect(meshcore: &MeshCore) {
    if let Err(err) = meshcore.disconnect().await {
        tracing::warn!(%err, "disconnect failed");
    }
}

/// The hop chain of the most recent channel packet the RF log reported.
struct PendingPath {
    path_len: u8,
    hashes: Vec<Vec<u8>>,
    at: Instant,
}

/// How long a logged path stays a candidate for the next channel message.
///
/// The radio pushes the RF log a few milliseconds before the fetched message,
/// so this only absorbs scheduling jitter; it is short enough that an unrelated
/// packet cannot be mistaken for the message's route.
const PATH_MATCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// Hop count from the companion protocol's packed path byte.
///
/// `ChannelMessage::path_len` is the encoded wire byte — bits 0-5 are the hop
/// count and bits 6-7 are the hash-size code (`hash_size - 1`), not a byte
/// count; the MeshCore packet format packs them (`0x45` is 5 hops with 2-byte
/// hashes, `0x40` is 0 hops with 2-byte hashes). `0xFF` means the packet arrived
/// via a direct route and carries no hop count. meshcore-rs 0.2.0 decodes this
/// correctly for the RF log but exposes the raw byte on `ChannelMessage`, so the
/// mask lives here until the crate does it.
fn hop_count(path_len: u8) -> u8 {
    if path_len == 0xFF { 0 } else { path_len & 0x3F }
}

/// Whether a logged path belongs to the message being handled.
///
/// Both the hop count and the age are checked: a path that does not match the
/// message's hop count is a different packet, and a stale one was not followed
/// by a message at all.
fn correlate(pending: &PendingPath, msg_path_len: u8, now: Instant) -> bool {
    pending.path_len == msg_path_len && now.duration_since(pending.at) < PATH_MATCH_WINDOW
}

/// Split a raw hop path into one hash per repeater.
///
/// `path` is `path_len * hash_size` bytes, one hash of `hash_size` bytes per
/// repeater. A `hash_size` of zero is refused rather than dividing the path into
/// infinite empty hops, and a truncated path yields only the hops it holds.
fn split_path(path: &[u8], path_len: u8, hash_size: u8) -> Vec<Vec<u8>> {
    if hash_size == 0 {
        return Vec::new();
    }
    let size = usize::from(hash_size);
    (0..usize::from(path_len))
        .filter_map(|i| {
            let start = i * size;
            path.get(start..start + size).map(<[u8]>::to_vec)
        })
        .collect()
}

/// Render a non-empty hop chain as `A > B > C`, using a name when one resolved
/// and the hop's hex id otherwise. The empty and uncorrelated cases are decided
/// by [`chain`], which is the only caller.
fn render_repeaters(hashes: &[Vec<u8>], names: &[Option<String>]) -> String {
    hashes
        .iter()
        .enumerate()
        .map(|(i, hash)| match names.get(i).and_then(|n| n.as_deref()) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => hex(hash),
        })
        .collect::<Vec<_>>()
        .join(" > ")
}

/// The chain to put on the air, from the correlated RF-log path.
///
/// `None` is "could not correlate" and reads `?`; a correlated path with no
/// hops is a real message that no repeater forwarded, and reads `direct`; else
/// it is the resolved chain. Split out so the three cases are testable with no
/// radio and no contacts.
fn chain(correlated: Option<&PendingPath>, resolved: &str) -> String {
    match correlated {
        None => "?".to_string(),
        Some(p) if p.hashes.is_empty() => "direct".to_string(),
        Some(_) => resolved.to_string(),
    }
}

/// Lowercase-free hex, so a hop id reads as a stable two-digit-per-byte token.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02X}");
    }
    out
}

/// What a message should cause.
///
/// The entire policy — parse, channel scope, verb resolution, the confirmation
/// latch — is decided by [`decide`], which does no I/O and never sees a
/// `MeshCore`. That is what makes the parts that must not be wrong testable on a
/// machine with no radio attached: `handle_message` is only the I/O around it.
enum Decision<'a> {
    /// Nothing to say and nothing to run: chat, or a message that tried to
    /// poison a reserved key.
    Ignore,
    Reply(String),
    Execute {
        action: &'a verbs::ActionSpec,
        /// Moved out of `decide` rather than borrowed: action arguments expand
        /// from the parsed message, and a borrow of a local would not outlive
        /// the call.
        ctx: parse::Context,
    },
}

impl Decision<'_> {
    /// The reply text, for asserting the on-air surface in tests.
    #[cfg(test)]
    fn reply(&self) -> Option<&str> {
        match self {
            Self::Ignore | Self::Execute { .. } => None,
            Self::Reply(reply) => Some(reply),
        }
    }

    #[cfg(test)]
    fn is_execute(&self) -> bool {
        matches!(self, Self::Execute { .. })
    }
}

/// Decide what one inbound message means. Pure apart from the latch's clock.
fn decide<'a>(
    table: &'a verbs::VerbTable,
    reserved: &parse::Reserved,
    latch: &mut latch::Latch,
    raw: &str,
    channel: &str,
    channel_idx: u8,
    meta: &parse::MessageMeta,
) -> Decision<'a> {
    // The marker decides whether this is a command at all. Everything below
    // works on the text *after* it, never on the raw message, so the sender tag
    // and the marker cannot leak into a verb name or an argument.
    let Some(command) = parse::command_text(raw) else {
        let body = parse::sender_body(raw);
        return if parse::parse_with(body, reserved).is_ok() {
            // Addressed to us and spelled correctly, but missing the marker. Say
            // so rather than staying silent: silence is indistinguishable from
            // the bot being down, which is how a genuine outage presents too.
            tracing::info!(channel_idx, "command without marker");
            Decision::Reply(format!("commands start with '{}'", parse::COMMAND_MARKER))
        } else {
            // Ordinary chat. Debug, not info: these channels are busy, and an
            // info line per message would bury the ones that matter.
            tracing::debug!(channel_idx, "not addressed as a command");
            Decision::Ignore
        };
    };

    let mut ctx = match parse::parse_with(command, reserved) {
        Ok(ctx) => ctx,
        Err(err) => {
            // Not a command, so no reply: these channels are not exclusively
            // ours, and answering every message that is not a command turns a
            // chatty channel into a bot that talks over itself.
            let hint = match err {
                ParseError::TooLong { .. } => "too long",
                _ => "not a command",
            };
            tracing::info!(channel_idx, %err, hint, "unparsable message");
            return Decision::Ignore;
        }
    };
    verbs::with_system(&mut ctx, channel, channel_idx, meta);

    if ctx.is_poisoned("channel") || ctx.is_poisoned("channel_idx") {
        // A relayed message tried to claim a different channel than the one it
        // arrived on. The real value is intact; any rule touching it fails to
        // match, so there is nothing to run. Silent, because the sender is
        // probing and telling them it failed is a free oracle.
        tracing::warn!(
            channel_idx,
            spoofed = ?ctx.poisoned_keys().collect::<Vec<_>>(),
            "message tried to override a reserved key"
        );
        return Decision::Ignore;
    }

    // Channel scope, checked before resolution so it covers every outcome and
    // not just the ones that spawn something. `reboot` belongs to #admin;
    // running it on #homeassistant would be a command on the wrong channel, and
    // even the read-only replies would be off-limits there.
    if let Some(verb) = ctx.verb().and_then(|name| table.get(name))
        && verb
            .channel
            .as_deref()
            .is_some_and(|scope| scope != channel)
    {
        tracing::info!(channel_idx, verb = %verb.name, "verb out of scope here");
        return Decision::Reply(format!("{verb}: not on this channel"));
    }

    match table.resolve(&ctx) {
        verbs::Resolution::Fire { verb, action, word } => {
            if action.confirm {
                if !ctx.confirmed() {
                    // Arm rather than run. Keyed on the canonical command, so
                    // `REBOOT  alpha` shares this state with `reboot alpha`
                    // rather than arming a second entry that never fires.
                    latch.arm(&ctx.canonical());
                    tracing::info!(
                        channel_idx,
                        verb = %verb.name,
                        key = %ctx.canonical(),
                        "armed, waiting for confirmation"
                    );
                    // The prompt echoes what was typed, not the canonical
                    // spelling: prompting `reboot alpha ok` after `reboot
                    // delta` would confirm a different machine.
                    //
                    // `command`, not `text`: the raw message still carries the
                    // sender tag and the marker, and echoing those would ask
                    // the user to confirm a string that can never parse.
                    return Decision::Reply(format!("confirm: '{}'", verbs::confirm_text(command)));
                }
                if !latch.take(&ctx.canonical()) {
                    // Expired, never armed, or a different key. Answer, and do
                    // not run: `reboot beta ok` must never be satisfied by an
                    // arming for `alpha`.
                    tracing::info!(channel_idx, key = %ctx.canonical(), "nothing armed");
                    return Decision::Reply("nothing armed - send the command again".to_string());
                }
                tracing::info!(channel_idx, verb = %verb.name, "confirmed");
            } else if ctx.confirmed() {
                // The parser recognises `ok` on any command, because it cannot
                // see the verb table. Here we can: a trailing `ok` on something
                // that is not gated is a mistake, and ignoring it would run the
                // command while the user believed they were confirming.
                tracing::info!(channel_idx, verb = %verb.name, "'ok' on a non-gated verb");
                return Decision::Reply("'ok' only follows a two-step command".to_string());
            }

            tracing::info!(
                channel_idx,
                verb = %verb.name,
                target = ?word,
                script = ?action.script,
                "running action"
            );
            Decision::Execute { action, ctx }
        }
        other => match other.into_reply() {
            Some(reply) => Decision::Reply(reply),
            None => Decision::Ignore,
        },
    }
}

/// How far the radio may fall behind before we bother writing to it.
///
/// A device that is a few seconds out has not drifted in any way that matters to
/// a mesh timestamp, and writing on every reconnect is churn for nothing. A
/// radio that has been powered down is minutes or years out, which is what this
/// exists to catch.
const CLOCK_SKEW_TOLERANCE_SECS: u32 = 60;

/// Whether the radio's clock is behind ours enough to be worth correcting.
///
/// The radio keeps no clock worth trusting — it loses time when it is powered
/// down — so on every connect this is compared against the container clock. Only
/// a device that is *behind* is written to: one that is ahead was set by hand,
/// and overwriting that would be the regression rather than the fix.
fn should_set_clock(device: u32, now: u32) -> bool {
    now.saturating_sub(device) > CLOCK_SKEW_TOLERANCE_SECS
}

fn now_secs() -> u32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    u32::try_from(secs).unwrap_or(u32::MAX)
}

/// Read the radio clock and correct it if it has fallen behind.
///
/// Called on every successful connect, which is the only moment worth a radio
/// round trip.
///
/// Best effort. A radio that will not answer a time query must not stop the bot
/// from answering messages, so a failure here is logged and the run continues.
/// Read the log to confirm the sync happened: `Device time: N, System time: M`
/// followed by either `Radio clock updated to: N` or `Device time is current or
/// ahead - no update needed`.
///
/// Note the companion radio sits behind the proxy, which keeps client sockets
/// open across a radio disconnect and reconnects the port itself. Nothing
/// breaks the TCP connection, so this function cannot rely on a disconnect to
/// be re-entered. [`run_connection`] polls the radio and redials when it stops
/// answering, which is what brings the run back here after a power-cycle.
async fn set_radio_clock(meshcore: &MeshCore) {
    let now = now_secs();
    // One lock for both commands: there is no await between the read and the
    // write, so nothing else can interleave, and a second acquisition would
    // open a window where a message handler ran in between.
    let commands = meshcore.commands().lock().await;

    let device = match commands.get_time().await {
        Ok(device) => device,
        Err(err) => {
            tracing::warn!(%err, "could not read the radio clock; leaving it as it is");
            return;
        }
    };
    tracing::info!("Device time: {device}, System time: {now}");

    if !should_set_clock(device, now) {
        tracing::info!("Device time is current or ahead - no update needed");
        return;
    }

    match commands.set_time(now).await {
        Ok(_) => tracing::info!("Radio clock updated to: {now}"),
        Err(err) => tracing::warn!(%err, "could not set the radio clock"),
    }
}

/// One inbound message, end to end. I/O only: [`decide`] has already decided.
// The parameters are the pieces of the decision plus the I/O handle; grouping
// them into a struct would only move the same fields behind one more name.
#[allow(clippy::too_many_arguments)]
async fn handle_message(
    meshcore: &MeshCore,
    config: &config::Loaded,
    latch: &mut latch::Latch,
    dir: &Path,
    text: &str,
    channel: &str,
    channel_idx: u8,
    meta: &parse::MessageMeta,
) {
    match decide(
        &config.table,
        &config.reserved,
        latch,
        text,
        channel,
        channel_idx,
        meta,
    ) {
        Decision::Ignore => {}
        Decision::Reply(reply) => send(meshcore, channel_idx, &[reply]).await,
        Decision::Execute { action, ctx } => {
            let outcome = script::run(action, &ctx, dir).await;
            let replies = render(action, &ctx, &outcome);
            send(meshcore, channel_idx, &replies).await;
        }
    }
}

/// Build the replies an action produces — one message per line of its template.
///
/// A failed action never gets to render its template: the template is where
/// `{{stdout}}` goes, and a partially-run action's output is not a result worth
/// broadcasting. A failure is therefore always a single message. A template that
/// cannot expand is a config bug, so it degrades to the generic failure line and
/// is logged loudly rather than panicking in the middle of the event loop.
fn render(
    action: &verbs::ActionSpec,
    ctx: &parse::Context,
    outcome: &script::Outcome,
) -> Vec<String> {
    if !outcome.success {
        return vec![outcome.failure_line()];
    }
    // Passed even when empty: an action that succeeds silently with a
    // `{{stdout}}` reply should say nothing rather than fail to render.
    match script::expand(&action.reply, ctx, Some(&outcome.stdout)) {
        Ok(reply) => verbs::split_reply(&reply),
        Err(err) => {
            tracing::warn!(script = ?action.script, %err, "reply template did not expand");
            vec!["action failed".to_string()]
        }
    }
}

/// Put each reply on the air, one message at a time.
///
/// `send_channel_msg` appends the bytes with no length check, so an over-long
/// reply hangs on the radio rather than truncating. The clamp is repeated here
/// even though `render` already did it: this is the last code that touches the
/// radio, so it is the last place a missing clamp would hurt.
///
/// Sequential, and a failure does not stop the rest. A partially-delivered list
/// is still more use than none of it, and airtime is too scarce to spend on a
/// retry of the whole thing.
async fn send(meshcore: &MeshCore, channel_idx: u8, replies: &[String]) {
    if replies.is_empty() {
        tracing::info!(channel_idx, "action produced no reply");
        return;
    }
    for reply in replies {
        let text = verbs::clamp(reply, verbs::MAX_REPLY_BYTES);
        if text.is_empty() {
            continue;
        }
        tracing::info!(channel_idx, %text, "replying");
        // Scoped so the command lock is released before the next await. Sequential
        // handling means nothing else contends for it, but holding a mutex across
        // unrelated awaits is a habit worth not forming.
        let result = {
            let commands = meshcore.commands().lock().await;
            commands.send_channel_msg(channel_idx, &text, None).await
        };
        if let Err(err) = result {
            // A failed reply must not take the bot down: the next message still
            // needs handling, and mesh airtime is scarce enough that a retry would
            // be worse than a miss.
            tracing::warn!(channel_idx, %err, "reply not sent");
        }
    }
}

/// What the startup readback found in one slot.
#[derive(Debug, PartialEq, Eq)]
enum Slot {
    /// A channel this bot listens on, with the name the config expects.
    Verified,
    /// Not a channel this bot listens on. Whether or not it holds a channel is the
    /// radio owner's business, so both cases land here.
    Unmonitored,
}

/// Decide what one slot means, given only its readback.
///
/// `read` is the name the radio reported, or `None` when `GET_CHANNEL` failed —
/// which is what an empty slot looks like from here.
///
/// The radio's channel table belongs to mc-webui. This bot only ever reads it, so
/// the asymmetry is the point: a slot we listen on has to be *proven* present and
/// correctly named, or the run stops; a slot we do not listen on is nobody's
/// business, whether or not it holds a channel.
///
/// Split out of [`verify_channels`] so the policy can be tested with no radio and
/// no `MeshCore`.
fn slot_verdict(idx: u8, read: Option<&str>, channels: &[(u8, String)]) -> Result<Slot> {
    let Some(expected) = channels
        .iter()
        .find(|(i, _)| *i == idx)
        .map(|(_, n)| n.as_str())
    else {
        return Ok(Slot::Unmonitored);
    };

    let Some(name) = read else {
        bail!(
            "channel {idx} is declared as {expected:?} but the radio would not report it; \
             refusing to start — this bot never creates or edits a channel, so set it up in \
             mc-webui"
        );
    };
    if name != expected {
        bail!(
            "channel {idx} is {name:?} but config expects {expected:?}; \
             the radio layout has drifted — refusing to start"
        );
    }
    Ok(Slot::Verified)
}

/// Read the radio's channel table back and fail if it disagrees with the config.
///
/// `channels` is the config's `bot.channels`, so the listen set and the startup
/// assertion are the same declaration: there is no second list to drift.
///
/// Strictly read-only: this issues `GET_CHANNEL` and nothing else. There is no
/// `SET_CHANNEL` anywhere in this crate, which is what keeps the bot out of the
/// proxy's channel virtualizer — the one command that remaps indices onto
/// allocator-chosen slots, and so would invalidate the map this function checks.
/// A test at the bottom of this file fails the build if that ever changes.
///
/// Every slot is read, including ones `bot.channels` does not name: a channel
/// mc-webui set up for the phone app or the family is read and left alone, never
/// tidied away.
async fn verify_channels(meshcore: &MeshCore, channels: &[(u8, String)]) -> Result<()> {
    let commands = meshcore.commands().lock().await;
    let mut unmonitored = 0u32;

    for idx in 0..config::CHANNEL_SLOTS {
        let read = match commands.get_channel(idx).await {
            Ok(info) => Some(info.name),
            Err(err) => {
                tracing::debug!(channel_idx = idx, %err, "slot holds no channel");
                None
            }
        };

        match slot_verdict(idx, read.as_deref(), channels)? {
            Slot::Verified => {
                let name = read.expect("a verified slot was read");
                tracing::info!(channel_idx = idx, %name, "channel verified");
            }
            Slot::Unmonitored => {
                if read.is_some() {
                    unmonitored += 1;
                }
            }
        }
    }

    if unmonitored > 0 {
        tracing::info!(
            unmonitored,
            "channels this bot does not listen on; read only, left as they are"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADMIN: &str = "#admin";
    const HA: &str = "#homeassistant";

    /// The verb table and the channel map, both from the shipped example config,
    /// so the tests assert against what the repository documents rather than
    /// against a table that only exists in Rust.
    fn config() -> &'static config::Loaded {
        config::example()
    }

    fn table() -> &'static verbs::VerbTable {
        &config().table
    }

    /// Run one command *body* through the whole policy and return what it
    /// decided. The marker is prepended here, so each test reads as the command
    /// rather than as the wire framing around it; the framing — the marker and
    /// the sender tag ahead of it — is covered by `parse`'s tests and by
    /// [`a_nickname_tagged_message_is_a_command`], which goes through [`run_raw`].
    fn run_on<'a>(
        table: &'a verbs::VerbTable,
        latch: &mut latch::Latch,
        text: &str,
        channel: &str,
    ) -> Decision<'a> {
        run_raw(table, latch, &format!("!{text}"), channel)
    }

    /// Run a raw message, marker and sender tag included.
    fn run_raw<'a>(
        table: &'a verbs::VerbTable,
        latch: &mut latch::Latch,
        raw: &str,
        channel: &str,
    ) -> Decision<'a> {
        let loaded = config();
        let idx = loaded
            .channels
            .iter()
            .find(|(_, n)| n == channel)
            .map(|(i, _)| *i)
            .expect("channel is in the listen set");
        decide(
            table,
            &loaded.reserved,
            latch,
            raw,
            channel,
            idx,
            &parse::MessageMeta::default(),
        )
    }

    // ---- the latch, which is the only thing standing between a message and a
    // ---- reboot. These are the tests that matter most in this file.

    /// The on-air shape: the app prepends the sender, then the marker, then the
    /// verb. `decide` must see `komodo`, not the tag and not the marker.
    #[test]
    fn a_nickname_tagged_message_is_a_command() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_raw(table, &mut latch, "NICKNAME: !komodo", ADMIN);
        assert!(d.is_execute(), "the tag and marker were not stripped");
    }

    #[test]
    fn reboot_needs_two_messages() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        let first = run_on(table, &mut latch, "reboot alpha", ADMIN);
        assert!(!first.is_execute(), "one message must not reboot");
        assert_eq!(first.reply(), Some("confirm: 'reboot alpha ok'"));

        let second = run_on(table, &mut latch, "reboot alpha ok", ADMIN);
        assert!(second.is_execute(), "the confirmation must run");
    }

    /// The whole point of keying on the canonical command: a phone keyboard that
    /// uppercases the lock key must not leave an arming nothing can consume.
    #[test]
    fn a_differently_spelled_confirmation_still_works() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        run_on(table, &mut latch, "reboot alpha", ADMIN);
        let second = run_on(table, &mut latch, "REBOOT  AlPhA  ok", ADMIN);
        assert!(second.is_execute());
    }

    /// The dangerous one: confirming `beta` must not be satisfied by an arming for
    /// `alpha`. It must answer, and it must leave the other arming intact.
    #[test]
    fn confirming_a_different_machine_does_not_run() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        run_on(table, &mut latch, "reboot alpha", ADMIN);
        let wrong = run_on(table, &mut latch, "reboot beta ok", ADMIN);
        assert!(!wrong.is_execute());
        assert_eq!(
            wrong.reply(),
            Some("nothing armed - send the command again")
        );
        assert!(latch.is_armed("reboot alpha"), "alpha was spent");

        // And the original arming still works afterwards.
        assert!(run_on(table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
    }

    #[test]
    fn a_confirmation_with_nothing_armed_does_not_run() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_on(table, &mut latch, "reboot alpha ok", ADMIN);
        assert!(!d.is_execute());
        assert!(d.reply().is_some());
    }

    /// An expired arming must not resurrect. Zero TTL makes the window empty
    /// without a sleep, so this is deterministic.
    #[test]
    fn an_expired_arming_does_not_run() {
        let table = table();
        let mut latch = latch::Latch::new(0);
        run_on(table, &mut latch, "reboot alpha", ADMIN);
        assert!(!run_on(table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
    }

    /// A replayed confirmation must not reboot twice.
    #[test]
    fn a_confirmation_runs_only_once() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        run_on(table, &mut latch, "reboot alpha", ADMIN);
        assert!(run_on(table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
        assert!(!run_on(table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
    }

    /// `garage open` is mutating but idempotent, so it runs in one message — and
    /// a stray `ok` must not become a second, separate path to that action.
    #[test]
    fn a_stray_ok_on_a_ungated_verb_does_nothing() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_on(table, &mut latch, "garage open ok", HA);
        assert!(!d.is_execute());
        assert_eq!(d.reply(), Some("'ok' only follows a two-step command"));
    }

    #[test]
    fn an_ungated_mutating_action_runs_in_one_message() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        assert!(run_on(table, &mut latch, "garage open", HA).is_execute());
        assert!(run_on(table, &mut latch, "alarm arm", HA).is_execute());
    }

    // ---- channel scope

    /// `reboot` is declared on #admin. On #homeassistant it must not arm, must
    /// not run, and must not leak its words through a suggestion.
    #[test]
    fn a_verb_cannot_run_on_the_wrong_channel() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        let bare = run_on(table, &mut latch, "reboot alpha", HA);
        assert!(!bare.is_execute());
        assert_eq!(bare.reply(), Some("reboot: not on this channel"));

        // Not even the confirmation path, and nothing was armed as a side effect.
        let confirmed = run_on(table, &mut latch, "reboot alpha ok", HA);
        assert!(!confirmed.is_execute());
        assert_eq!(latch.armed_count(), 0, "out-of-scope armed something");

        // And on the right channel it still arms.
        assert!(!run_on(table, &mut latch, "reboot alpha", ADMIN).is_execute());
        assert_eq!(latch.armed_count(), 1);
    }

    #[test]
    fn ha_verbs_are_refused_on_the_admin_channel() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        for input in ["garage", "garage open", "alarm arm"] {
            let d = run_on(table, &mut latch, input, ADMIN);
            assert!(!d.is_execute(), "{input} ran on {ADMIN}");
        }
    }

    /// `help` is not in the verb table, so it has no scope and must answer
    /// anywhere — otherwise there is no way to ask what is possible.
    #[test]
    fn help_works_on_either_channel() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        for channel in [ADMIN, HA] {
            let d = run_on(table, &mut latch, "help", channel);
            assert!(d.reply().is_some(), "no help on {channel}");
        }
    }

    // ---- reserved-key poisoning

    /// A message claiming a different channel must never reach an action, and
    /// must not be told that it failed.
    #[test]
    fn a_channel_spoof_is_dropped_silently() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        for input in [
            "reboot alpha channel=#homeassistant",
            "reboot alpha channel_idx=9",
            "garage channel=#admin",
        ] {
            let d = run_on(table, &mut latch, input, ADMIN);
            assert!(!d.is_execute(), "{input} ran");
            assert_eq!(d.reply(), None, "{input} was answered");
        }
    }

    /// A spoofed `target` is poisoning, so the real target survives — but the
    /// verb must still not match, or the spoof would be a way to redirect a
    /// reboot while looking successful.
    #[test]
    fn a_target_spoof_does_not_change_what_runs() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_on(table, &mut latch, "reboot target=beta", ADMIN);
        assert!(!d.is_execute());
    }

    // ---- no reply where there is nothing to say

    /// The parser is verb-shaped, not sentence-shaped: any first word is a verb
    /// guess, so even `"hi there"` becomes verb `hi` with target `there`. Silence
    /// therefore has two sources — input the parser rejects, and a verb guess
    /// that is not near enough to any declared verb to be worth correcting.
    #[test]
    fn input_the_parser_rejects_gets_no_reply() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let too_long = "a".repeat(65);
        for input in [
            "",
            "line\nbreak",
            "tab\there",
            &too_long,
            "reboot alpha beta",
        ] {
            let d = run_on(table, &mut latch, input, HA);
            assert!(!d.is_execute(), "{input:?} ran");
            assert_eq!(d.reply(), None, "{input:?} was answered");
        }
    }

    /// A near-miss earns a pointer; a word that is not a near-miss gets nothing.
    /// The parser is verb-shaped, not sentence-shaped, so every one-word message
    /// becomes a verb guess — and these channels are not exclusively ours, so a
    /// reply to every greeting is a bot talking over itself.
    #[test]
    fn only_an_unknown_word_near_a_verb_gets_an_answer() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        // Near enough to `garage` to be worth correcting.
        let near = run_on(table, &mut latch, "gerage", HA);
        assert!(near.reply().expect("a pointer").contains("garage"));

        // Not near anything: silence, not a correction nobody asked for.
        for input in ["hello", "bonjour", "meteo"] {
            let d = run_on(table, &mut latch, input, HA);
            assert!(!d.is_execute(), "{input} ran");
            assert_eq!(d.reply(), None, "{input} was answered");
        }
    }

    #[test]
    fn a_typo_gets_a_pointer_not_a_rejection() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_on(table, &mut latch, "gerage", HA);
        let reply = d.reply().expect("a suggestion");
        assert!(reply.contains("garage"), "{reply}");
    }

    #[test]
    fn the_clock_is_only_written_when_the_radio_is_behind() {
        let now = 1_700_000_000;
        // A radio that has been off since the epoch: the case that matters.
        assert!(should_set_clock(0, now));
        // Weeks out.
        assert!(should_set_clock(now - 30 * 86_400, now));
        // Just out of tolerance.
        assert!(should_set_clock(now - CLOCK_SKEW_TOLERANCE_SECS - 1, now));
        // Agreed, or within jitter: leave it alone.
        assert!(!should_set_clock(now, now));
        assert!(!should_set_clock(now - CLOCK_SKEW_TOLERANCE_SECS, now));
        // Ahead: somebody set it by hand, and that is not ours to undo.
        assert!(!should_set_clock(now + 10_000, now));
    }

    #[test]
    fn the_system_clock_reads_as_sane() {
        // 2024-01-01, so a machine with no clock at all fails visibly rather
        // than quietly syncing the radio to 1970.
        assert!(now_secs() > 1_704_067_200, "{}", now_secs());
    }

    // ---- the channel table is read-only

    fn listen_set() -> Vec<(u8, String)> {
        vec![(2, "#admin".to_string()), (3, "#homeassistant".to_string())]
    }

    /// The declared channels have to be there, with the name the config expects.
    #[test]
    fn a_declared_channel_that_reads_back_correctly_is_verified() {
        let channels = listen_set();
        for (idx, name) in &channels {
            assert_eq!(
                slot_verdict(*idx, Some(name), &channels).unwrap(),
                Slot::Verified
            );
        }
    }

    /// A renamed channel is a radio re-layout, not something to correct. Writing
    /// the expected name back is exactly the write this bot must never make.
    #[test]
    fn a_declared_channel_with_the_wrong_name_is_fatal() {
        let err = slot_verdict(2, Some("#family"), &listen_set())
            .unwrap_err()
            .to_string();
        assert!(err.contains("#family"), "{err}");
        assert!(err.contains("refusing to start"), "{err}");
    }

    /// The new fail-closed rule: a channel we listen on that the radio will not
    /// report stops the run. It used to be skipped, which left the bot listening on
    /// a slot it never checked.
    #[test]
    fn a_declared_channel_the_radio_will_not_report_is_fatal() {
        let err = slot_verdict(2, None, &listen_set())
            .unwrap_err()
            .to_string();
        assert!(err.contains("would not report it"), "{err}");
        // The message has to say where the fix belongs, because the bot will not do it.
        assert!(err.contains("mc-webui"), "{err}");
    }

    /// The other half of the invariant: a channel mc-webui configured that this
    /// config does not name is read once and left alone, whether or not it exists.
    #[test]
    fn an_undeclared_channel_is_left_alone() {
        let channels = listen_set();
        assert_eq!(
            slot_verdict(0, Some("#family"), &channels).unwrap(),
            Slot::Unmonitored
        );
        assert_eq!(slot_verdict(7, None, &channels).unwrap(), Slot::Unmonitored);
    }

    /// The whole hard invariant, checked against the source rather than trusted:
    /// the radio's channel and contact tables are shared with mc-webui, the phone
    /// app and the family, and this bot may only read them.
    ///
    /// `SET_CHANNEL` is the command that would engage the proxy's channel
    /// virtualizer and remap indices onto physical slots — which is not a bug in
    /// this service's own map, but an overwrite of somebody else's channels. It
    /// is one method call away at any time, and nothing about it fails to compile
    /// or to test, so the guarantee has to be mechanical.
    ///
    /// The needles are assembled at runtime so this test does not match itself, and
    /// each keeps its leading dot so prose about these calls in a comment or a doc
    /// line is not mistaken for one. Each is `.` + `prefix` + `_` + `method` + `(`:
    /// a needle that misses one of those parts silently matches nothing, which is
    /// the shape of a guard that looks like it works and does not.
    #[test]
    fn nothing_in_this_crate_writes_the_radio_channel_or_contact_table() {
        const FORBIDDEN: [(&str, &str); 4] = [
            ("set", "channel"),
            ("add", "contact"),
            ("remove", "contact"),
            ("set", "flood_scope"),
        ];

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&src).expect("src/ is readable") {
            let path = entry.expect("readable dir entry").path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable source file");
            checked += 1;
            for (prefix, method) in FORBIDDEN {
                let needle = format!(".{prefix}_{method}(");
                assert!(
                    !text.contains(&needle),
                    "{} calls {prefix}_{method}() — the radio's channel table belongs to \
                     mc-webui and is read-only to this bot",
                    path.display()
                );
            }
        }
        assert!(checked >= 6, "only scanned {checked} files in src/");
    }

    // ---- reply rendering

    fn ctx_for(input: &str) -> parse::Context {
        parse::parse(input).unwrap()
    }

    /// A reply that is meant to be a single message.
    ///
    /// Asserting the count separately is what makes a regression legible: a
    /// script that starts printing two lines fails here as "expected one
    /// message" rather than as a slice comparison that never matches.
    fn only(replies: Vec<String>) -> String {
        assert_eq!(replies.len(), 1, "expected one message, got {replies:?}");
        replies.into_iter().next().unwrap()
    }

    #[test]
    fn a_failed_action_never_renders_its_template() {
        let action = verbs::ActionSpec {
            script: Some("boom.sh".into()),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            // A partial result from a script that then failed. Broadcasting this
            // would report a half-finished reboot as done.
            stdout: "rebooting\n".into(),
            stderr: String::new(),
            success: false,
            // No spawn error: the script ran and exited non-zero, which is a
            // different thing from having failed to start.
            error: None,
        };
        assert_eq!(
            only(render(&action, &ctx_for("reboot alpha"), &outcome)),
            "action did not succeed"
        );
    }

    #[test]
    fn a_missing_script_reports_a_generic_failure() {
        let action = verbs::ActionSpec {
            script: Some("missing.sh".into()),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: String::new(),
            stderr: String::new(),
            success: false,
            error: Some("script \"missing.sh\" not found in /data".into()),
        };
        // The error names a host path, which is not something to put on a mesh.
        assert_eq!(
            only(render(&action, &ctx_for("alarm"), &outcome)),
            "action failed"
        );
    }

    #[test]
    fn a_successful_literal_reply_is_its_own_text() {
        let action = verbs::ActionSpec {
            script: Some("ha-service.sh".into()),
            reply: "opening".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: "called cover.open_cover, http 200".into(),
            stderr: String::new(),
            success: true,
            error: None,
        };
        assert_eq!(
            only(render(&action, &ctx_for("garage open"), &outcome)),
            "opening"
        );
    }

    /// A template that cannot expand is a config bug. It must degrade rather
    /// than panic inside the event loop.
    #[test]
    fn an_unexpandable_template_degrades() {
        let action = verbs::ActionSpec {
            script: Some("ha-entity.sh".into()),
            reply: "{{nope}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: "armed".into(),
            stderr: String::new(),
            success: true,
            error: None,
        };
        assert_eq!(
            only(render(&action, &ctx_for("alarm"), &outcome)),
            "action failed"
        );
    }

    /// An action that succeeds silently and replies `{{stdout}}` has nothing to
    /// say, and `send` drops the empty reply rather than putting a blank frame
    /// on the air.
    #[test]
    fn silence_renders_as_no_reply() {
        let action = verbs::ActionSpec {
            script: Some("quiet.sh".into()),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: String::new(),
            stderr: String::new(),
            success: true,
            error: None,
        };
        assert!(render(&action, &ctx_for("alarm"), &outcome).is_empty());
    }

    /// The last thing before the radio. A script that prints a paragraph must
    /// not produce a frame the radio cannot send, and printing more than one
    /// thing must not produce more messages than the budget allows.
    #[test]
    fn every_reply_fits_in_one_frame() {
        let action = verbs::ActionSpec {
            script: Some("loud.sh".into()),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: "x".repeat(400),
            stderr: String::new(),
            success: true,
            error: None,
        };
        let replies = render(&action, &ctx_for("alarm"), &outcome);
        assert!(
            replies.len() <= verbs::MAX_REPLIES + 1,
            "{} messages",
            replies.len()
        );
        for reply in &replies {
            assert!(
                reply.len() <= verbs::MAX_REPLY_BYTES,
                "{} bytes",
                reply.len()
            );
        }
    }

    /// The point of the feature: a script that reports several things puts one
    /// on the air per line, rather than being truncated to a single frame.
    #[test]
    fn each_line_of_stdout_becomes_its_own_message() {
        let action = verbs::ActionSpec {
            script: Some("komodo-status.sh".into()),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: "1/3 server biniou not ok\n2/3 stack meshcore unhealthy\n3/3 stack ha down\n"
                .into(),
            stderr: String::new(),
            success: true,
            error: None,
        };
        assert_eq!(
            render(&action, &ctx_for("komodo"), &outcome),
            [
                "1/3 server biniou not ok",
                "2/3 stack meshcore unhealthy",
                "3/3 stack ha down"
            ]
        );
    }

    // ---- the RF-log hop path

    #[test]
    fn split_path_cuts_the_path_into_one_hash_per_hop() {
        // Three hops of two-byte hashes.
        let path = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(
            split_path(&path, 3, 2),
            vec![vec![0xAA, 0xBB], vec![0xCC, 0xDD], vec![0xEE, 0xFF]]
        );
    }

    /// A zero hash size would divide the path into infinite empty hops.
    #[test]
    fn split_path_refuses_a_zero_hash_size() {
        assert!(split_path(&[1, 2, 3, 4], 2, 0).is_empty());
    }

    /// A truncated log yields only the hops it actually holds.
    #[test]
    fn split_path_yields_only_the_hops_it_holds() {
        assert_eq!(split_path(&[0xAA], 3, 1), vec![vec![0xAA]]);
    }

    #[test]
    fn render_repeaters_prefers_names_and_falls_back_to_hex() {
        let hashes = vec![vec![0xAA, 0xBB], vec![0xCC, 0xDD], vec![0xEE, 0xFF]];
        // Resolved, unresolved, and an empty name all in one chain.
        let names = vec![Some("NODE-A".to_string()), None, Some(String::new())];
        assert_eq!(render_repeaters(&hashes, &names), "NODE-A > CCDD > EEFF");
    }

    /// `ChannelMessage::path_len` is the packed wire byte: bits 0-5 hop count,
    /// bits 6-7 hash-size code. `0xFF` is a direct route with no hop count. This
    /// is the bug that made a zero-hop message report `64 hops`.
    #[test]
    fn hop_count_decodes_the_packed_path_byte() {
        assert_eq!(hop_count(0x00), 0); // 0 hops, 1-byte hashes
        assert_eq!(hop_count(0x05), 5); // 5 hops, 1-byte hashes
        assert_eq!(hop_count(0x40), 0); // 0 hops, 2-byte hashes
        assert_eq!(hop_count(0x45), 5); // 5 hops, 2-byte hashes
        assert_eq!(hop_count(0x8A), 10); // 10 hops, 3-byte hashes
        assert_eq!(hop_count(0xFF), 0); // direct route
    }

    #[test]
    fn chain_is_direct_for_a_zero_hop_message() {
        let p = pending(0, std::time::Duration::from_millis(1));
        assert_eq!(chain(Some(&p), ""), "direct");
    }

    #[test]
    fn chain_is_question_mark_when_not_correlated() {
        assert_eq!(chain(None, ""), "?");
    }

    #[test]
    fn chain_is_the_resolved_chain_when_correlated() {
        let p = PendingPath {
            path_len: 2,
            hashes: vec![vec![0xAA], vec![0xBB]],
            at: Instant::now(),
        };
        assert_eq!(chain(Some(&p), "A > B"), "A > B");
    }

    fn pending(path_len: u8, age: std::time::Duration) -> PendingPath {
        PendingPath {
            path_len,
            hashes: Vec::new(),
            at: Instant::now() - age,
        }
    }

    #[test]
    fn correlate_requires_the_same_hop_count_and_a_fresh_log() {
        let now = Instant::now();
        assert!(correlate(
            &pending(3, std::time::Duration::from_millis(10)),
            3,
            now
        ));
        // A different hop count is a different packet.
        assert!(!correlate(
            &pending(3, std::time::Duration::from_millis(10)),
            4,
            now
        ));
        // Too old: no message followed it.
        assert!(!correlate(
            &pending(3, PATH_MATCH_WINDOW + std::time::Duration::from_secs(1)),
            3,
            now
        ));
    }

    /// End to end through `render`: a reply-only action (no script) builds its
    /// answer entirely from the injected metadata.
    #[test]
    fn a_reply_only_action_renders_from_the_message_metadata() {
        let action = verbs::ActionSpec {
            script: None,
            reply: "ping: {{delay}}s, {{hops}} hops | {{repeaters}}".into(),
            ..verbs::ActionSpec::default()
        };
        let mut ctx = ctx_for("ping");
        verbs::with_system(
            &mut ctx,
            ADMIN,
            2,
            &parse::MessageMeta {
                path_len: 3,
                sender_timestamp: 1000,
                repeaters: "A > B > C".into(),
                now: 1012,
                ..parse::MessageMeta::default()
            },
        );
        let outcome = script::Outcome {
            stdout: String::new(),
            stderr: String::new(),
            success: true,
            error: None,
        };
        assert_eq!(
            only(render(&action, &ctx, &outcome)),
            "ping: 12s, 3 hops | A > B > C"
        );
    }
}
