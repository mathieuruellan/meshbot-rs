//! meshbot-rs — MeshCore channel rule/action bot.
//!
//! Connects to the radio over TCP, verifies the channel map it was configured
//! with, then streams channel messages. A message is parsed into a command
//! context, resolved against the verb table, and either answered or run as the
//! script the verb table names for it.
//!
//! Not yet wired: the config loader, so [`verbs::default_verbs`] is still the
//! vocabulary rather than `config.yaml`. See `verbs::default_verbs`.

mod latch;
mod parse;
mod script;
mod verbs;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::StreamExt;
use meshcore_rs::{EventPayload, EventType, MeshCore};
use parse::ParseError;

/// Physical channel index -> expected channel name.
///
/// These are the *radio's* real indices. We never send `SET_CHANNEL`, so the
/// proxy's virtualizer never maps us and the indices pass through unchanged.
const CHANNELS: &[(u8, &str)] = &[(3, "#admin"), (4, "#homeassistant")];

/// How many channel slots to read back when verifying the map.
const CHANNEL_SLOTS: u8 = 8;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    dotenvy::dotenv().ok();

    let host = std::env::var("MESHCORE_HOST").unwrap_or_else(|_| "proxy".to_string());
    let port: u16 = std::env::var("MESHCORE_PORT")
        .unwrap_or_else(|_| "5000".to_string())
        .parse()
        .context("MESHCORE_PORT must be a number")?;

    tracing::info!(version = env!("CARGO_PKG_VERSION"), host = %host, port, "starting");

    let meshcore = MeshCore::tcp(&host, port)
        .await
        .with_context(|| format!("cannot connect to {host}:{port}"))?;

    let info = meshcore
        .commands()
        .lock()
        .await
        .send_appstart()
        .await
        .context("send_appstart failed")?;
    tracing::info!(name = %info.name, "app started");

    verify_channels(&meshcore).await?;

    // Returns unit, not a Result — there is nothing to await that can fail.
    meshcore.start_auto_message_fetching().await;
    tracing::info!("listening for channel messages");

    let table = verbs::default_verbs();
    let script_dir = script_dir()?;
    let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

    let mut stream = meshcore.event_stream_filtered(EventType::ChannelMsgRecv);
    while let Some(event) = stream.next().await {
        if let EventPayload::ChannelMessage(msg) = event.payload {
            let Some((_, name)) = CHANNELS.iter().find(|(idx, _)| *idx == msg.channel_idx) else {
                // Not in the listen set. `CHANNELS` is both the filter and the
                // startup assertion, so a message from anywhere else is dropped
                // before it is even parsed.
                tracing::debug!(channel_idx = msg.channel_idx, "channel not monitored");
                continue;
            };
            tracing::info!(
                channel_idx = msg.channel_idx,
                message_id = msg.message_id(),
                text = %msg.text,
                "channel message"
            );
            // Sequential, deliberately. One message is resolved, executed and
            // answered before the next is read, so two actions cannot interleave
            // their scripts or their replies.
            handle_message(
                &meshcore,
                &table,
                &mut latch,
                &script_dir,
                &msg.text,
                name,
                msg.channel_idx,
            )
            .await;
        }
    }

    Ok(())
}

/// The script allowlist directory, checked once at startup.
///
/// A missing directory is a hard startup failure rather than a warning: the
/// compose file mounts it, so its absence means the mount is wrong, and a bot
/// that answers `garage` with "action failed" while looking healthy hides the
/// real problem. `MESHBOT_SCRIPT_DIR` overrides it, which is how the tracked
/// templates in `scripts.example/` are exercised without a deploy.
fn script_dir() -> Result<PathBuf> {
    let dir = script::script_dir();
    std::fs::canonicalize(&dir).with_context(|| {
        format!(
            "action script directory {} is not usable; set MESHBOT_SCRIPT_DIR to override",
            dir.display()
        )
    })?;
    tracing::info!(dir = %dir.display(), "script allowlist directory");
    Ok(dir)
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
    latch: &mut latch::Latch,
    text: &str,
    channel: &str,
    channel_idx: u8,
) -> Decision<'a> {
    let mut ctx = match parse::parse(text) {
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
    verbs::with_system(&mut ctx, channel, channel_idx);

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
                    return Decision::Reply(format!("confirm: '{}'", verbs::confirm_text(text)));
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
                script = %action.script,
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

/// One inbound message, end to end. I/O only: [`decide`] has already decided.
async fn handle_message(
    meshcore: &MeshCore,
    table: &verbs::VerbTable,
    latch: &mut latch::Latch,
    dir: &Path,
    text: &str,
    channel: &str,
    channel_idx: u8,
) {
    match decide(table, latch, text, channel, channel_idx) {
        Decision::Ignore => {}
        Decision::Reply(reply) => send(meshcore, channel_idx, &reply).await,
        Decision::Execute { action, ctx } => {
            let outcome = script::run(action, &ctx, dir).await;
            let reply = render(action, &ctx, &outcome);
            send(meshcore, channel_idx, &reply).await;
        }
    }
}

/// Build the one reply an action produces.
///
/// A failed action never gets to render its template: the template is where
/// `{{stdout}}` goes, and a partially-run action's output is not a result worth
/// broadcasting. A template that cannot expand is a config bug, so it degrades
/// to the generic failure line and is logged loudly rather than panicking in the
/// middle of the event loop.
fn render(action: &verbs::ActionSpec, ctx: &parse::Context, outcome: &script::Outcome) -> String {
    if !outcome.success {
        return outcome.failure_line();
    }
    // Passed even when empty: an action that succeeds silently with a
    // `{{stdout}}` reply should say nothing rather than fail to render.
    match script::expand(&action.reply, ctx, Some(&outcome.stdout)) {
        Ok(reply) => verbs::clamp(&reply, verbs::MAX_REPLY_BYTES),
        Err(err) => {
            tracing::warn!(script = %action.script, %err, "reply template did not expand");
            "action failed".to_string()
        }
    }
}

/// Put one reply on the air.
///
/// `send_channel_msg` appends the bytes with no length check, so an over-long
/// reply hangs on the radio rather than truncating. The clamp is repeated here
/// even though `render` already did it: this is the last code that touches the
/// radio, so it is the last place a missing clamp would hurt.
async fn send(meshcore: &MeshCore, channel_idx: u8, reply: &str) {
    let text = verbs::clamp(reply, verbs::MAX_REPLY_BYTES);
    if text.is_empty() {
        tracing::info!(channel_idx, "action produced no reply");
        return;
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

/// Read the radio's channel table back and fail if it disagrees with `CHANNELS`.
///
/// Read-only: this issues `GET_CHANNEL` only. `SET_CHANNEL` would be the one
/// call that puts us in the proxy's virtualizer and remaps indices.
async fn verify_channels(meshcore: &MeshCore) -> Result<()> {
    let commands = meshcore.commands().lock().await;

    for idx in 0..CHANNEL_SLOTS {
        let info = match commands.get_channel(idx).await {
            Ok(info) => info,
            Err(err) => {
                // Empty slots are expected to fail or come back blank; only a
                // slot we actually care about is a hard failure.
                tracing::debug!(channel_idx = idx, %err, "no channel in slot");
                continue;
            }
        };

        let expected = CHANNELS.iter().find(|(i, _)| *i == idx).map(|(_, n)| *n);
        match expected {
            Some(name) if info.name == name => {
                tracing::info!(channel_idx = idx, name = %info.name, "channel verified");
            }
            Some(name) => {
                anyhow::bail!(
                    "channel {idx} is {:?} but this build expects {name:?}; \
                     the radio layout has drifted — refusing to start",
                    info.name
                );
            }
            None => tracing::info!(channel_idx = idx, name = %info.name, "unmonitored channel"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADMIN: &str = "#admin";
    const HA: &str = "#homeassistant";

    fn table() -> verbs::VerbTable {
        verbs::default_verbs()
    }

    /// Run one message through the whole policy and return what it decided.
    fn run_on<'a>(
        table: &'a verbs::VerbTable,
        latch: &mut latch::Latch,
        text: &str,
        channel: &str,
    ) -> Decision<'a> {
        let idx = CHANNELS
            .iter()
            .find(|(_, n)| *n == channel)
            .map(|(i, _)| *i)
            .unwrap();
        decide(table, latch, text, channel, idx)
    }

    // ---- the latch, which is the only thing standing between a message and a
    // ---- reboot. These are the tests that matter most in this file.

    #[test]
    fn reboot_needs_two_messages() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        let first = run_on(&table, &mut latch, "reboot alpha", ADMIN);
        assert!(!first.is_execute(), "one message must not reboot");
        assert_eq!(first.reply(), Some("confirm: 'reboot alpha ok'"));

        let second = run_on(&table, &mut latch, "reboot alpha ok", ADMIN);
        assert!(second.is_execute(), "the confirmation must run");
    }

    /// The whole point of keying on the canonical command: a phone keyboard that
    /// uppercases the lock key must not leave an arming nothing can consume.
    #[test]
    fn a_differently_spelled_confirmation_still_works() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        run_on(&table, &mut latch, "reboot alpha", ADMIN);
        let second = run_on(&table, &mut latch, "REBOOT  AlPhA  ok", ADMIN);
        assert!(second.is_execute());
    }

    /// The dangerous one: confirming `beta` must not be satisfied by an arming for
    /// `alpha`. It must answer, and it must leave the other arming intact.
    #[test]
    fn confirming_a_different_machine_does_not_run() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        run_on(&table, &mut latch, "reboot alpha", ADMIN);
        let wrong = run_on(&table, &mut latch, "reboot beta ok", ADMIN);
        assert!(!wrong.is_execute());
        assert_eq!(
            wrong.reply(),
            Some("nothing armed - send the command again")
        );
        assert!(latch.is_armed("reboot alpha"), "alpha was spent");

        // And the original arming still works afterwards.
        assert!(run_on(&table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
    }

    #[test]
    fn a_confirmation_with_nothing_armed_does_not_run() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_on(&table, &mut latch, "reboot alpha ok", ADMIN);
        assert!(!d.is_execute());
        assert!(d.reply().is_some());
    }

    /// An expired arming must not resurrect. Zero TTL makes the window empty
    /// without a sleep, so this is deterministic.
    #[test]
    fn an_expired_arming_does_not_run() {
        let table = table();
        let mut latch = latch::Latch::new(0);
        run_on(&table, &mut latch, "reboot alpha", ADMIN);
        assert!(!run_on(&table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
    }

    /// A replayed confirmation must not reboot twice.
    #[test]
    fn a_confirmation_runs_only_once() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        run_on(&table, &mut latch, "reboot alpha", ADMIN);
        assert!(run_on(&table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
        assert!(!run_on(&table, &mut latch, "reboot alpha ok", ADMIN).is_execute());
    }

    /// `garage open` is mutating but idempotent, so it runs in one message — and
    /// a stray `ok` must not become a second, separate path to that action.
    #[test]
    fn a_stray_ok_on_a_ungated_verb_does_nothing() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_on(&table, &mut latch, "garage open ok", HA);
        assert!(!d.is_execute());
        assert_eq!(d.reply(), Some("'ok' only follows a two-step command"));
    }

    #[test]
    fn an_ungated_mutating_action_runs_in_one_message() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        assert!(run_on(&table, &mut latch, "garage open", HA).is_execute());
        assert!(run_on(&table, &mut latch, "alarm arm", HA).is_execute());
    }

    // ---- channel scope

    /// `reboot` is declared on #admin. On #homeassistant it must not arm, must
    /// not run, and must not leak its words through a suggestion.
    #[test]
    fn a_verb_cannot_run_on_the_wrong_channel() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);

        let bare = run_on(&table, &mut latch, "reboot alpha", HA);
        assert!(!bare.is_execute());
        assert_eq!(bare.reply(), Some("reboot: not on this channel"));

        // Not even the confirmation path, and nothing was armed as a side effect.
        let confirmed = run_on(&table, &mut latch, "reboot alpha ok", HA);
        assert!(!confirmed.is_execute());
        assert_eq!(latch.armed_count(), 0, "out-of-scope armed something");

        // And on the right channel it still arms.
        assert!(!run_on(&table, &mut latch, "reboot alpha", ADMIN).is_execute());
        assert_eq!(latch.armed_count(), 1);
    }

    #[test]
    fn ha_verbs_are_refused_on_the_admin_channel() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        for input in ["garage", "garage open", "alarm arm"] {
            let d = run_on(&table, &mut latch, input, ADMIN);
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
            let d = run_on(&table, &mut latch, "help", channel);
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
            let d = run_on(&table, &mut latch, input, ADMIN);
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
        let d = run_on(&table, &mut latch, "reboot target=beta", ADMIN);
        assert!(!d.is_execute());
    }

    // ---- no reply where there is nothing to say

    /// The parser is verb-shaped, not sentence-shaped: any first word is a verb
    /// guess, so even `"hi there"` becomes verb `hi` with target `there` and gets
    /// a pointer. Silence therefore happens only for input the parser rejects —
    /// empty, control characters, over the byte cap, or a third bare word.
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
            let d = run_on(&table, &mut latch, input, HA);
            assert!(!d.is_execute(), "{input:?} ran");
            assert_eq!(d.reply(), None, "{input:?} was answered");
        }
    }

    /// One bare word is a verb-shaped guess, so it earns a pointer. This is the
    /// "typos get a pointer, not a rejection" rule, and it is why the silence
    /// test above has to use input the parser rejects rather than any word.
    #[test]
    fn a_single_unknown_word_gets_a_pointer() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        for (input, expected) in [("hello", None), ("gerage", Some("garage"))] {
            let d = run_on(&table, &mut latch, input, HA);
            let reply = d.reply().expect("a reply");
            if let Some(expected) = expected {
                assert!(reply.contains(expected), "{reply}");
            } else {
                assert!(reply.contains("try 'help'"), "{reply}");
            }
        }
    }

    #[test]
    fn a_typo_gets_a_pointer_not_a_rejection() {
        let table = table();
        let mut latch = latch::Latch::new(latch::CONFIRM_TTL_SECS);
        let d = run_on(&table, &mut latch, "gerage", HA);
        let reply = d.reply().expect("a suggestion");
        assert!(reply.contains("garage"), "{reply}");
    }

    // ---- reply rendering

    fn ctx_for(input: &str) -> parse::Context {
        parse::parse(input).unwrap()
    }

    #[test]
    fn a_failed_action_never_renders_its_template() {
        let action = verbs::ActionSpec {
            script: "boom.sh".into(),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            // A partial result from a script that then failed. Broadcasting this
            // would report a half-finished reboot as done.
            stdout: "rebooting\n".into(),
            success: false,
            // No spawn error: the script ran and exited non-zero, which is a
            // different thing from having failed to start.
            error: None,
        };
        let reply = render(&action, &ctx_for("reboot alpha"), &outcome);
        assert_eq!(reply, "action did not succeed");
    }

    #[test]
    fn a_missing_script_reports_a_generic_failure() {
        let action = verbs::ActionSpec {
            script: "missing.sh".into(),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: String::new(),
            success: false,
            error: Some("script \"missing.sh\" not found in /data".into()),
        };
        // The error names a host path, which is not something to put on a mesh.
        assert_eq!(
            render(&action, &ctx_for("alarm"), &outcome),
            "action failed"
        );
    }

    #[test]
    fn a_successful_literal_reply_is_its_own_text() {
        let action = verbs::ActionSpec {
            script: "ha-service.sh".into(),
            reply: "opening".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: "called cover.open_cover, http 200".into(),
            success: true,
            error: None,
        };
        assert_eq!(
            render(&action, &ctx_for("garage open"), &outcome),
            "opening"
        );
    }

    /// A template that cannot expand is a config bug. It must degrade rather
    /// than panic inside the event loop.
    #[test]
    fn an_unexpandable_template_degrades() {
        let action = verbs::ActionSpec {
            script: "ha-entity.sh".into(),
            reply: "{{nope}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: "armed".into(),
            success: true,
            error: None,
        };
        assert_eq!(
            render(&action, &ctx_for("alarm"), &outcome),
            "action failed"
        );
    }

    /// An action that succeeds silently and replies `{{stdout}}` has nothing to
    /// say, and `send` drops the empty reply rather than putting a blank frame
    /// on the air.
    #[test]
    fn silence_renders_as_no_reply() {
        let action = verbs::ActionSpec {
            script: "quiet.sh".into(),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: String::new(),
            success: true,
            error: None,
        };
        assert_eq!(render(&action, &ctx_for("alarm"), &outcome), "");
    }

    /// The last thing before the radio. A script that prints a paragraph must
    /// not produce a frame the radio cannot send.
    #[test]
    fn every_reply_fits_in_one_frame() {
        let action = verbs::ActionSpec {
            script: "loud.sh".into(),
            reply: "{{stdout}}".into(),
            ..verbs::ActionSpec::default()
        };
        let outcome = script::Outcome {
            stdout: "x".repeat(400),
            success: true,
            error: None,
        };
        let reply = render(&action, &ctx_for("alarm"), &outcome);
        assert!(
            reply.len() <= verbs::MAX_REPLY_BYTES,
            "{} bytes",
            reply.len()
        );
    }
}
