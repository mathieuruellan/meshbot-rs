//! The on-disk configuration: `config.yaml`, read once at startup.
//!
//! Everything the bot can do is data here — which channels it listens on, which
//! verbs exist, which script each action runs. There is no built-in verb table to
//! drift out of sync with the host; what the operator wrote is what runs.
//!
//! Validation is deliberately fatal. A typo in a key, a verb scoped to a channel
//! the bot does not listen on, a script that is not in the allowlist: all of
//! those are startup errors, not degraded runtime. A bot that starts with half
//! its config understood is worse than one that refuses to start, because the
//! half it dropped is a command the operator believes is live.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;

use crate::latch;
use crate::parse::Reserved;
use crate::script;
use crate::verbs::{ActionSpec, VerbSpec, VerbTable};

/// The only schema this build speaks. A config written for a different one is
/// refused rather than partially applied.
///
/// Schema 2 moved the help-surface narrowing from `bot.help_list_only` onto the
/// channel entry it describes (`quiet: true`), which is why a schema 1 config is
/// refused rather than read leniently.
pub const SCHEMA: u64 = 2;

/// Host location, overridable so the example config runs without a deploy.
pub const DEFAULT_CONFIG_PATH: &str = "/data/meshcore/meshbot-rs/config.yaml";

/// The radio stores a channel name in 32 bytes with a NUL terminator, so a name
/// longer than this is silently cut on the way in.
const CHANNEL_NAME_MAX_BYTES: usize = 31;

/// How many channel slots the radio exposes, and therefore how many the startup
/// readback covers.
///
/// This is the one number both the declaration and the verification depend on: an
/// index at or past it would be a channel the bot listens on and never checks, so
/// it is refused at load rather than quietly unverified.
pub const CHANNEL_SLOTS: u8 = 8;

/// The parsed config, before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u64,
    bot: Bot,
    verbs: Vec<VerbSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bot {
    /// Physical radio channel index -> the name the radio reports for it.
    /// Matching is by index *and* name, so a stale index fails loudly at
    /// startup instead of listening to a channel that was renumbered.
    channels: BTreeMap<u8, ChannelSpec>,
    /// Where action scripts live. `MESHBOT_SCRIPT_DIR` overrides this, so the
    /// examples in this repo run without a deploy.
    script_dir: PathBuf,
    /// How long a gated command stays armed waiting for its `… ok`, in seconds.
    /// Omitted means [`crate::latch::CONFIRM_TTL_SECS`].
    #[serde(default)]
    confirm_ttl_secs: Option<u64>,
}

/// One entry of `bot.channels`: the name the radio holds, and how much the bot
/// is willing to say there.
///
/// Two spellings, so a channel with nothing to configure stays one line:
///
/// ```yaml
/// channels:
///   2: "ruellan-family #biniou-monitori"   # shorthand, quiet defaults to false
///   0:
///     name: "#bot"
///     quiet: true
/// ```
///
/// The shorthand is the common case — most channels are ordinary — and the map
/// form is only reached for the ones that set something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSpec {
    /// The channel name exactly as the radio reports it, truncation included.
    pub name: String,
    /// Narrow the help surface: the bare `help` answers, a named topic does not,
    /// no reply points at help, and a message missing the `!` marker is chat
    /// rather than a nudge. Commands still answer — see [`crate::verbs::HelpMode`].
    pub quiet: bool,
}

impl<'de> Deserialize<'de> for ChannelSpec {
    /// Accept either spelling of a channel entry.
    ///
    /// A visitor rather than `#[serde(untagged)]`: untagged reports
    /// "data did not match any variant" for a mistyped key, which names neither
    /// the channel nor the key the operator got wrong. Hand-written, the map arm
    /// can name the key — a real message reads
    /// `bot.channels.3: unknown key "quite", expected 'name' or 'quiet' at
    /// line 4 column 8`, with the path and location supplied by the deserializer.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ChannelSpecVisitor;

        impl<'de> serde::de::Visitor<'de> for ChannelSpecVisitor {
            type Value = ChannelSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a channel name, or a mapping of `name` and optional `quiet`")
            }

            /// `0: "#bot"` — the name alone, full help surface.
            fn visit_str<E>(self, name: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ChannelSpec {
                    name: name.to_string(),
                    quiet: false,
                })
            }

            /// `0: {name: "#bot", quiet: true}` — the name plus options.
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut name: Option<String> = None;
                let mut quiet: Option<bool> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            if name.is_some() {
                                return Err(serde::de::Error::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        "quiet" => {
                            if quiet.is_some() {
                                return Err(serde::de::Error::duplicate_field("quiet"));
                            }
                            quiet = Some(map.next_value()?);
                        }
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown key {other:?}, expected 'name' or 'quiet'"
                            )));
                        }
                    }
                }

                let name = name.ok_or_else(|| serde::de::Error::missing_field("name"))?;
                Ok(ChannelSpec {
                    name,
                    quiet: quiet.unwrap_or(false),
                })
            }
        }

        deserializer.deserialize_any(ChannelSpecVisitor)
    }
}

/// A validated config, ready to hand to the rest of the bot.
#[derive(Debug)]
pub struct Loaded {
    /// Listen set, by radio index.
    pub channels: Vec<(u8, String)>,
    pub table: VerbTable,
    /// The `.env` names a template or script can reach, which a message may not
    /// set as a slot.
    pub reserved: Reserved,
    /// The canonicalized directory every script name in `table` was validated
    /// against. Handed back rather than re-resolved by the caller, so a script
    /// cannot be validated in one directory and then executed in another.
    pub script_dir: PathBuf,
    /// The confirmation window the latch is built with, in seconds. Resolved
    /// from `bot.confirm_ttl_secs`, or the default when the key is absent.
    pub confirm_ttl_secs: u64,
    /// Channels declared `quiet: true`, derived from `bot.channels` itself.
    ///
    /// Derived rather than declared, so there is no second list to drift out of
    /// agreement with the listen set: an index can only be here if it is also in
    /// `channels`, which `validate_channels` has already range-checked.
    pub quiet: BTreeSet<u8>,
}

impl Loaded {
    /// The help mode of the channel a message arrived on.
    ///
    /// The one place the index set becomes a policy, so `decide` and the tests
    /// cannot disagree about which channels are quiet.
    pub fn help_mode(&self, channel_idx: u8) -> crate::verbs::HelpMode {
        if self.quiet.contains(&channel_idx) {
            crate::verbs::HelpMode::Quiet
        } else {
            crate::verbs::HelpMode::Full
        }
    }
}

/// Read, parse, validate, and prepare the config. The script directory is
/// canonicalized first, so `resolve_script` can compare against it.
pub fn load(path: &Path) -> Result<Loaded> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;

    let parsed: Config = serde_norway::from_str(&raw)
        .with_context(|| format!("cannot parse config {}", path.display()))?;

    let dir = effective_script_dir(&parsed.bot);
    let dir = dir.canonicalize().with_context(|| {
        format!(
            "script directory {} is not usable; create it or set MESHBOT_SCRIPT_DIR",
            dir.display()
        )
    })?;

    build(parsed, dir).with_context(|| format!("invalid config {}", path.display()))
}

/// Validate a parsed config against the script directory, already canonicalized.
///
/// Split from [`load`] so tests exercise the real validation without a
/// deployment on disk, and without setting a process-wide env var to redirect
/// the script directory.
fn build(cfg: Config, dir: PathBuf) -> Result<Loaded> {
    if cfg.schema != SCHEMA {
        bail!(
            "config declares schema {} but this build speaks schema {SCHEMA}",
            cfg.schema
        );
    }

    let channels = validate_channels(&cfg.bot.channels)?;
    let reserved = validate_verbs(&cfg.verbs, &channels, &dir)?;
    let confirm_ttl_secs = validate_confirm_ttl(cfg.bot.confirm_ttl_secs)?;
    // Derived from the same map `channels` came from, so the two cannot disagree:
    // every index here was range-checked by `validate_channels` a line above.
    let quiet = cfg
        .bot
        .channels
        .iter()
        .filter(|(_, spec)| spec.quiet)
        .map(|(idx, _)| *idx)
        .collect();

    Ok(Loaded {
        channels,
        table: VerbTable::new(cfg.verbs),
        reserved,
        script_dir: dir,
        confirm_ttl_secs,
        quiet,
    })
}

/// Resolve the confirmation window, refusing the one value that is not a window.
///
/// Zero would arm nothing that a later message could ever spend, so every gated
/// action would become unrunnable while still *looking* gated — `reboot` would
/// answer `confirm: …` forever and never reboot. That is a silent loss of the
/// only destructive gate, so it is a startup error naming the key rather than a
/// value an operator can pick by omitting a digit.
fn validate_confirm_ttl(ttl: Option<u64>) -> Result<u64> {
    match ttl {
        None => Ok(latch::CONFIRM_TTL_SECS),
        Some(0) => bail!(
            "bot.confirm_ttl_secs is 0; a confirmation window of zero seconds can never be \
             met, so every gated action would be permanently unrunnable. Omit the key for the \
             {}s default",
            latch::CONFIRM_TTL_SECS
        ),
        Some(ttl) => Ok(ttl),
    }
}

fn effective_script_dir(bot: &Bot) -> PathBuf {
    match std::env::var("MESHBOT_SCRIPT_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => bot.script_dir.clone(),
    }
}

/// Parse and validate a config held in memory.
///
/// Test-facing: the binary always goes through [`load`], which adds the file
/// read and the script-directory resolution. This exists so the schema can be
/// exercised — including against the tracked example — without a deployment on
/// disk and without a process-wide env var to redirect the directory.
#[cfg(test)]
pub fn from_yaml(raw: &str, dir: &Path) -> Result<Loaded> {
    let parsed: Config = serde_norway::from_str(raw).context("cannot parse config")?;
    build(parsed, dir.to_path_buf())
}

/// The tracked example config, parsed and validated once.
///
/// One fixture for every test that needs a real verb table. It loads the file
/// the repository ships, so a schema change that breaks `config.example.yaml`
/// fails the test suite rather than quietly making the documentation a lie.
#[cfg(test)]
pub fn example() -> &'static Loaded {
    use std::sync::OnceLock;

    static EXAMPLE: OnceLock<Loaded> = OnceLock::new();
    EXAMPLE.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts.example");
        from_yaml(
            include_str!("../config.example.yaml"),
            &dir.canonicalize().expect("scripts.example is present"),
        )
        .expect("config.example.yaml matches the schema")
    })
}

/// The listen set, checked for the things that make a channel unusable.
///
/// The `quiet` flag rides along unchecked here by design: it is a property of an
/// entry that passed every check below, so unlike the deleted `bot.help_list_only`
/// it cannot name an index this function would reject.
fn validate_channels(channels: &BTreeMap<u8, ChannelSpec>) -> Result<Vec<(u8, String)>> {
    ensure!(
        !channels.is_empty(),
        "bot.channels is empty: nothing to listen on"
    );

    let mut names: BTreeSet<&str> = BTreeSet::new();
    for (idx, spec) in channels {
        // A channel the startup readback never reaches is a channel nobody
        // verifies, and the bot will not create it: an out-of-range index is a
        // config mistake to fix here, not a slot to invent at the radio.
        ensure!(
            *idx < CHANNEL_SLOTS,
            "bot.channels has index {idx}, which is outside the {CHANNEL_SLOTS} slots the radio \
             exposes; nothing would ever verify it"
        );

        let name = spec.name.trim();
        ensure!(
            !name.is_empty(),
            "bot.channels has an empty name for index {idx}"
        );
        ensure!(
            names.insert(name),
            "bot.channels names {name:?} twice; one name cannot be two indices"
        );
        if name.len() > CHANNEL_NAME_MAX_BYTES {
            bail!(
                "bot.channels[{idx}] name {name:?} is {} bytes, the radio stores {CHANNEL_NAME_MAX_BYTES} \
                 and truncates without complaint",
                name.len()
            );
        }
        if name.len() == CHANNEL_NAME_MAX_BYTES {
            // Already at the limit, so any future edit is a silent change. Worth
            // saying out loud rather than letting someone discover it via a
            // channel that no longer matches.
            tracing::warn!(
                "bot.channels[{idx}] name {name:?} is exactly {CHANNEL_NAME_MAX_BYTES} bytes; \
                 the radio cannot store a longer one"
            );
        }
    }
    Ok(channels
        .iter()
        .map(|(i, spec)| (*i, spec.name.trim().to_string()))
        .collect())
}

/// Every verb, checked against the listen set and the script allowlist. Returns
/// the `.env` names that must be reserved against message-supplied slots.
fn validate_verbs(verbs: &[VerbSpec], channels: &[(u8, String)], dir: &Path) -> Result<Reserved> {
    ensure!(
        !verbs.is_empty(),
        "verbs is empty: there is nothing to dispatch"
    );

    let listened: BTreeSet<&str> = channels.iter().map(|(_, n)| n.as_str()).collect();
    let mut reserved: BTreeSet<String> = BTreeSet::new();
    let mut seen_verbs: BTreeSet<String> = BTreeSet::new();

    for verb in verbs {
        let name = verb.name.trim();
        ensure!(!name.is_empty(), "a verb has an empty name");
        ensure!(
            seen_verbs.insert(name.to_ascii_lowercase()),
            "verb {name:?} is declared twice"
        );

        let channel = verb.channel.as_deref().unwrap_or_default().trim();
        ensure!(
            !channel.is_empty(),
            "verb {name:?} has no channel; every verb must be scoped to one"
        );
        ensure!(
            listened.contains(channel),
            "verb {name:?} is scoped to {channel:?}, which is not in bot.channels; \
             listening on {}",
            listened.iter().cloned().collect::<Vec<_>>().join(", ")
        );

        // `arg_pattern` takes the verb's one positional argument: it gates the
        // argument rather than sitting beside declared words, so the two can
        // never both be reachable — and a verb with nothing to fire once the
        // argument passed would be gating nothing. Checked before the general
        // "can never fire" rule below so the error names the key at fault.
        if let Some(pattern) = &verb.arg_pattern {
            ensure!(
                verb.args.is_empty(),
                "verb {name:?} declares arg_pattern and args; the pattern takes the one \
                 positional argument, so those words could never be reached"
            );
            ensure!(
                verb.get.is_some(),
                "verb {name:?} declares arg_pattern but no get; there would be nothing to fire \
                 once the argument matched"
            );
            ensure!(
                !pattern.as_str().is_empty(),
                "verb {name:?} has an empty arg_pattern, which matches every argument; omit the \
                 key if requiring an argument is all it should do"
            );
            // The argument the pattern gates is free message text, and the
            // parser has put it in the context as `target` — that is how the
            // verb saw it in the first place. `{{target}}` is therefore the one
            // placeholder that would carry it into a reply or a script argv,
            // which is exactly what a pattern-gated verb must not do.
            if let Some(get) = &verb.get {
                let leaks_arg = uses_placeholder(&get.reply, "target")
                    || get.args.iter().any(|a| uses_placeholder(a, "target"));
                ensure!(
                    !leaks_arg,
                    "verb {name:?} declares arg_pattern, so its argument is free message text; \
                     {{{{target}}}} would put it straight into the action"
                );
            }
        }

        ensure!(
            verb.get.is_some() || !verb.args.is_empty(),
            "verb {name:?} has neither a get nor any args, so it can never fire"
        );

        let mut seen_words: BTreeSet<String> = BTreeSet::new();
        if let Some(get) = &verb.get {
            validate_action(name, "get", get, dir, &mut reserved)?;
        }
        for (i, arg) in verb.args.iter().enumerate() {
            ensure!(
                !arg.words.is_empty(),
                "verb {name:?} arg {i} declares no words"
            );
            for word in &arg.words {
                let word = word.trim();
                ensure!(!word.is_empty(), "verb {name:?} arg {i} has an empty word");
                // `insert` is the assertion: it is true only the first time a
                // word is seen, so a repeat is what fails.
                ensure!(
                    seen_words.insert(word.to_ascii_lowercase()),
                    "verb {name:?} accepts {word:?} twice"
                );
            }
            validate_action(
                name,
                &format!("arg {}", arg.words.join("/")),
                &arg.action,
                dir,
                &mut reserved,
            )?;
        }
    }

    Ok(Reserved::new(reserved))
}

/// One action: the script must exist inside the allowlist, every template
/// placeholder must be one the engine fills, and the flags must make sense.
fn validate_action(
    verb: &str,
    which: &str,
    action: &ActionSpec,
    dir: &Path,
    reserved: &mut BTreeSet<String>,
) -> Result<()> {
    let at = format!("verb {verb:?} {which}");

    // A reply-only action names no script. It must then build its reply from
    // the context alone, so `{{stdout}}` — which only a script can fill — is
    // refused rather than rendered empty.
    match &action.script {
        Some(script) => {
            script::resolve_script(script, dir)
                .with_context(|| format!("{at}: script {script:?}"))?;
        }
        None => {
            ensure!(
                !action.reply.contains("{{stdout}}"),
                "{at}: has no script, so {{{{stdout}}}} can never be filled"
            );
        }
    }

    ensure!(
        !action.confirm || action.mutating,
        "{at}: confirm is set without mutating, so the latch would gate a read"
    );

    for arg in &action.args {
        check_template(&at, arg, &["target"])?;
    }
    check_template(
        &at,
        &action.reply,
        &[
            "target",
            "stdout",
            "sender",
            "hops",
            "delay",
            "snr",
            "sender_timestamp",
            "repeaters",
        ],
    )?;

    for name in &action.env {
        let name = name.trim();
        ensure!(!name.is_empty(), "{at}: env has an empty name");
        // Names are looked up in the process environment at spawn, so anything
        // outside that shape can never arrive anyway.
        ensure!(
            !name.contains('=') && !name.contains(char::is_whitespace),
            "{at}: env name {name:?} is not a valid environment name"
        );
        reserved.insert(name.to_ascii_lowercase());
    }

    Ok(())
}

/// Whether `template` references `{{name}}`.
///
/// Used where the question is "does this template read that value" rather than
/// "may it": a match on the whole placeholder, so `{{targetx}}` is not
/// `{{target}}`.
fn uses_placeholder(template: &str, name: &str) -> bool {
    template.contains(&format!("{{{{{name}}}}}"))
}

/// Reject a template the engine cannot fill.
///
/// `${NAME}` is rejected outright rather than left inert: `.env` is read with
/// `dotenvy`, which does not expand it, so a template using it would ship the
/// literal `${NAME}` to the air — the kind of thing that reads as a config bug
/// months later.
fn check_template(at: &str, template: &str, allowed: &[&str]) -> Result<()> {
    ensure!(
        !template.contains("${"),
        "{at}: template {template:?} uses ${{...}}; .env is not expanded, so this would be sent literally"
    );

    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find("}}") else {
            bail!("{at}: template {template:?} has an unterminated {{{{");
        };
        let name = rest[..end].trim();
        ensure!(
            allowed.contains(&name),
            "{at}: template {template:?} uses {{{{{name}}}}}; only {} can be expanded",
            allowed
                .iter()
                .map(|a| format!("{{{{{a}}}}}"))
                .collect::<Vec<_>>()
                .join(" and ")
        );
        rest = &rest[end + 2..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verbs::HelpMode;

    /// A scratch script directory with one script in it, so `resolve_script`
    /// has something real to find.
    fn scripts() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("meshbot-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("ok.sh");
        if !script.exists() {
            std::fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn config(verbs: &str) -> Result<Loaded> {
        from_yaml(
            &format!(
                "schema: 2\nbot:\n  channels: {{2: admin, 3: home}}\n  script_dir: /unused\nverbs:\n{verbs}"
            ),
            &scripts(),
        )
    }

    /// As [`config`], with the channel declared at one index instead of two, for
    /// the rules that are about the index rather than the name.
    fn config_at(idx: u8, verbs: &str) -> Result<Loaded> {
        from_yaml(
            &format!(
                "schema: 2\nbot:\n  channels: {{{idx}: admin}}\n  script_dir: /unused\nverbs:\n{verbs}"
            ),
            &scripts(),
        )
    }

    #[test]
    fn accepts_a_minimal_valid_config() {
        let loaded = config(
            "  - name: reboot\n    desc: restart\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
        )
        .unwrap();
        assert_eq!(
            loaded.channels,
            vec![(2, "admin".to_string()), (3, "home".to_string())]
        );
        assert!(loaded.table.get("reboot").is_some());
    }

    #[test]
    fn missing_script_is_fatal() {
        let err = config(
            "  - name: reboot\n    channel: admin\n    get:\n      script: nope.sh\n      reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nope.sh"), "{err}");
    }

    #[test]
    fn env_dollar_brace_is_rejected() {
        let err = config(
            "  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: \"${TOKEN}\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not expanded"), "{err}");
    }

    #[test]
    fn unknown_placeholder_is_rejected() {
        let err = config(
            "  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: \"{{nope}}\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("only {{target}} and {{stdout}}"), "{err}");
    }

    #[test]
    fn stdout_is_not_available_in_an_argument() {
        // `{{stdout}}` is the script's output, which does not exist yet at the
        // moment arguments are built.
        let err = config(
            "  - name: reboot\n    channel: admin\n    args:\n      - words: [a]\n        action:\n          script: ok.sh\n          args: [\"{{stdout}}\"]\n          reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("only {{target}}"), "{err}");
    }

    /// A reply-only action needs no script; it builds its answer from the
    /// message context alone. This is what `ping` uses.
    #[test]
    fn a_reply_only_action_needs_no_script() {
        let loaded = config(
            "  - name: ping\n    channel: admin\n    get:\n      reply: \"ping: {{delay}}s, {{hops}} hops | {{repeaters}}\"\n",
        )
        .unwrap();
        assert!(loaded.table.get("ping").is_some());
    }

    /// Without a script there is nothing to fill `{{stdout}}`, so it is a config
    /// error rather than a reply that silently renders empty.
    #[test]
    fn stdout_in_a_reply_only_action_is_fatal() {
        let err =
            config("  - name: ping\n    channel: admin\n    get:\n      reply: \"{{stdout}}\"\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("stdout"), "{err}");
    }

    /// The message-metadata placeholders are accepted alongside `{{target}}`.
    #[test]
    fn metadata_placeholders_are_accepted() {
        for placeholder in ["hops", "delay", "snr", "sender_timestamp", "repeaters"] {
            let raw = format!(
                "  - name: ping\n    channel: admin\n    get:\n      script: ok.sh\n      reply: \"{{{{{placeholder}}}}}\"\n"
            );
            assert!(config(&raw).is_ok(), "{{{{{placeholder}}}}} was refused");
        }
    }

    #[test]
    fn verb_on_an_unlistened_channel_is_fatal() {
        let err = config(
            "  - name: reboot\n    channel: elsewhere\n    get:\n      script: ok.sh\n      reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not in bot.channels"), "{err}");
    }

    #[test]
    fn a_verb_with_no_channel_is_fatal() {
        let err = config("  - name: reboot\n    get:\n      script: ok.sh\n      reply: up\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no channel"), "{err}");
    }

    #[test]
    fn a_verb_with_nothing_to_fire_is_fatal() {
        let err = config("  - name: reboot\n    channel: admin\n    desc: nothing\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("can never fire"), "{err}");
    }

    #[test]
    fn a_mistyped_key_is_fatal() {
        // The whole reason `deny_unknown_fields` is on every struct: a config
        // typed by hand on a host is where a silent typo hides a timeout.
        assert!(
            config(
                "  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n      tiomeout_secs: 5\n"
            )
            .is_err()
        );
    }

    #[test]
    fn confirm_without_mutating_is_fatal() {
        let err = config(
            "  - name: reboot\n    channel: admin\n    args:\n      - words: [a]\n        action:\n          script: ok.sh\n          mutating: false\n          confirm: true\n          reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("gate a read"), "{err}");
    }

    #[test]
    fn env_names_are_reserved_against_message_slots() {
        let loaded = config(
            "  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      env: [HA_TOKEN]\n      reply: up\n",
        )
        .unwrap();
        let ctx = crate::parse::parse_with("reboot ha_token=x", &loaded.reserved).unwrap();
        assert!(
            ctx.is_poisoned("ha_token"),
            "a slot shadowed a real env name"
        );
    }

    #[test]
    fn a_repeated_word_is_fatal() {
        // Two enum entries claiming the same word means `garage open` resolves
        // to whichever the table happens to list first.
        let err = config(
            "  - name: garage\n    channel: admin\n    args:\n      - words: [open, up]\n        action:\n          script: ok.sh\n          reply: a\n      - words: [open]\n        action:\n          script: ok.sh\n          reply: b\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("accepts \"open\" twice"), "{err}");
    }

    #[test]
    fn a_repeated_verb_name_is_fatal() {
        let err = config(
            "  - name: garage\n    channel: admin\n    get:\n      script: ok.sh\n      reply: a\n  - name: garage\n    channel: admin\n    get:\n      script: ok.sh\n      reply: b\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("declared twice"), "{err}");
    }

    #[test]
    fn one_channel_name_cannot_be_two_indices() {
        let raw = "schema: 2\nbot:\n  channels: {2: admin, 3: admin}\n  script_dir: /unused\nverbs:\n  - name: garage\n    channel: admin\n    get:\n      script: ok.sh\n      reply: a\n";
        let err = from_yaml(raw, &scripts()).unwrap_err().to_string();
        assert!(err.contains("twice"), "{err}");
    }

    /// The startup readback covers slots `0..CHANNEL_SLOTS` and nothing else, so a
    /// higher index would be a channel the bot listens on and never checks. The
    /// radio table belongs to mc-webui: the fix is to declare the real index, not
    /// for the bot to bring a slot into being.
    #[test]
    fn an_index_the_startup_readback_cannot_reach_is_fatal() {
        let err = config_at(
            9,
            "  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("outside the 8 slots"), "{err}");

        // The last slot the readback does reach is fine.
        assert!(config_at(
            CHANNEL_SLOTS - 1,
            "  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
        )
        .is_ok());
    }

    /// The directory handed back must be the one the scripts were checked in.
    /// It used to be re-resolved by the caller from `MESHBOT_SCRIPT_DIR` and a
    /// hardcoded default, so a config naming a different `bot.script_dir` was
    /// validated in one directory and would have been executed in another.
    #[test]
    fn the_validated_directory_is_the_one_returned() {
        let loaded = config("  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n").unwrap();
        assert_eq!(loaded.script_dir, scripts());
    }

    /// The window is config, and the key is optional: a host config written
    /// before the key existed must still start, on the built-in default.
    #[test]
    fn an_absent_confirm_ttl_uses_the_default() {
        let loaded = config("  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n").unwrap();
        assert_eq!(loaded.confirm_ttl_secs, latch::CONFIRM_TTL_SECS);
    }

    /// A configured window is carried through verbatim; it is the number the
    /// latch is built with and the number the prompt quotes.
    #[test]
    fn a_configured_confirm_ttl_is_used() {
        let loaded = from_yaml(
            "schema: 2\nbot:\n  channels: {2: admin}\n  script_dir: /unused\n  confirm_ttl_secs: 600\nverbs:\n  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
            &scripts(),
        )
        .unwrap();
        assert_eq!(loaded.confirm_ttl_secs, 600);
    }

    /// Zero would arm a confirmation nothing could ever complete, so the one
    /// destructive gate would look armed and never fire.
    #[test]
    fn a_zero_confirm_ttl_is_fatal() {
        let err = from_yaml(
            "schema: 2\nbot:\n  channels: {2: admin}\n  script_dir: /unused\n  confirm_ttl_secs: 0\nverbs:\n  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
            &scripts(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("confirm_ttl_secs"), "{err}");
        assert!(err.contains("permanently unrunnable"), "{err}");
    }

    /// `quiet` is optional and defaults off, so a config that never mentions it
    /// starts with every channel on the full help surface — and the shorthand
    /// spelling, which cannot set it at all, is how the common case reads.
    #[test]
    fn an_absent_quiet_means_full_help_everywhere() {
        let loaded = config("  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n").unwrap();
        assert!(loaded.quiet.is_empty());
        assert_eq!(loaded.help_mode(2), HelpMode::Full);
        assert_eq!(loaded.help_mode(3), HelpMode::Full);
    }

    /// A channel declared `quiet: true` resolves to the quiet mode; its
    /// neighbour, which did not, keeps full help.
    #[test]
    fn a_quiet_channel_is_quiet_and_the_others_are_not() {
        let loaded = from_yaml(
            "schema: 2\nbot:\n  channels:\n    2: admin\n    3: {name: home, quiet: true}\n  script_dir: /unused\nverbs:\n  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
            &scripts(),
        )
        .unwrap();
        assert_eq!(loaded.quiet, BTreeSet::from([3]));
        assert_eq!(loaded.help_mode(3), HelpMode::Quiet);
        assert_eq!(loaded.help_mode(2), HelpMode::Full);
    }

    /// A key that is not `name` or `quiet` is a typo for one of those two, so
    /// the load names the key rather than matching the entry against a variant
    /// and reporting that nothing fit. This is the specificity
    /// `deny_unknown_fields` gives everywhere else in this file.
    #[test]
    fn an_unknown_key_in_a_channel_entry_is_named() {
        let err = from_yaml(
            "schema: 2\nbot:\n  channels:\n    3: {name: home, quite: true}\n  script_dir: /unused\nverbs:\n  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
            &scripts(),
        )
        .unwrap_err();
        // `{:#}` walks anyhow's chain: the parse error is the *source* of
        // "cannot parse config", and only the source names the key.
        let err = format!("{err:#}");
        assert!(err.contains("quite"), "{err}");
        assert!(err.contains("quiet"), "{err}");
    }

    /// The map spelling is the only one that can set options, and it has no
    /// default for the name: an entry with nothing to identify it is a config
    /// error, not an anonymous channel.
    #[test]
    fn a_channel_entry_without_a_name_is_refused() {
        let err = from_yaml(
            "schema: 2\nbot:\n  channels:\n    3: {quiet: true}\n  script_dir: /unused\nverbs:\n  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
            &scripts(),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("name"), "{err}");
    }

    /// The old key is gone and this build speaks schema 2, so neither spelling
    /// of the old configuration starts. The message must name the key the
    /// operator actually wrote.
    #[test]
    fn the_removed_help_list_only_key_is_refused() {
        let err = from_yaml(
            "schema: 2\nbot:\n  channels: {2: admin}\n  script_dir: /unused\n  help_list_only: [2]\nverbs:\n  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
            &scripts(),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("help_list_only"), "{err}");
    }

    /// A schema 1 config — every config written before this change — is refused
    /// outright rather than read with the wrong understanding of its channels.
    #[test]
    fn schema_1_is_refused() {
        let err = from_yaml(
            "schema: 1\nbot:\n  channels: {2: admin}\n  script_dir: /unused\nverbs:\n  - name: reboot\n    channel: admin\n    get:\n      script: ok.sh\n      reply: up\n",
            &scripts(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("schema 1"), "{err}");
        assert!(err.contains("schema 2"), "{err}");
    }

    /// The shipped example is the schema's documentation, so it is validated by
    /// the build rather than trusted. `config.example.yaml` names a host path
    /// for `script_dir`, so the tracked example scripts are passed instead.
    #[test]
    fn the_shipped_example_is_valid() {
        let loaded = example();
        assert!(!loaded.channels.is_empty());
        assert!(loaded.table.get("reboot").is_some());
        // The example declares the window outright rather than relying on the
        // default, so a change to either side shows up here.
        assert_eq!(loaded.confirm_ttl_secs, 60);
    }

    // ---- `arg_pattern`

    #[test]
    fn an_arg_pattern_is_compiled_at_load() {
        let loaded = config(
            "  - name: ping\n    channel: admin\n    arg_pattern: nantes\n    get:\n      reply: up\n",
        )
        .unwrap();
        let verb = loaded.table.get("ping").expect("ping is declared");
        let pattern = verb.arg_pattern.as_ref().expect("carried through");
        // Case-insensitivity is set by the compiler, not written in the pattern.
        assert!(pattern.matches("Nantes-44"));
        assert!(!pattern.matches("rennes"));
    }

    /// A pattern that does not compile must stop the bot at startup rather than
    /// fail per message — silently, since a refused argument is never answered.
    ///
    /// `{err:#}` because anyhow's `Display` is only the outermost context; the
    /// loader wraps the parse error, and the reason is the reason.
    #[test]
    fn an_invalid_arg_pattern_is_fatal() {
        let err = config(
            "  - name: ping\n    channel: admin\n    arg_pattern: \"(\"\n    get:\n      reply: up\n",
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("arg_pattern"), "{err}");
    }

    /// An empty pattern matches everything, so the key would say "require an
    /// argument" in the least legible way possible. Refused, with the reason
    /// and the simpler thing to do instead.
    #[test]
    fn an_empty_arg_pattern_is_fatal() {
        let err = config(
            "  - name: ping\n    channel: admin\n    arg_pattern: \"\"\n    get:\n      reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty arg_pattern"), "{err}");
        assert!(err.contains("omit the key"), "{err}");
    }

    /// The pattern takes the verb's one positional argument, so declared words
    /// beside it could never fire — a config that asks for both is a mistake,
    /// not a precedence rule to guess at.
    #[test]
    fn arg_pattern_cannot_share_a_verb_with_declared_words() {
        let err = config(
            "  - name: garage\n    channel: admin\n    arg_pattern: open\n    get:\n      script: ok.sh\n      reply: up\n    args:\n      - words: [open]\n        action:\n          script: ok.sh\n          reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("arg_pattern and args"), "{err}");
    }

    /// Without a `get` there is nothing to run once the argument passes, and
    /// the declared words are refused anyway — so the verb would gate itself.
    #[test]
    fn arg_pattern_without_a_get_is_fatal() {
        let err = config("  - name: ping\n    channel: admin\n    arg_pattern: nantes\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("arg_pattern but no get"), "{err}");
    }

    /// The argument the pattern gates is free message text sitting in the
    /// context as `target`, so `{{target}}` is the one placeholder that would
    /// carry it into a reply or a script argv.
    #[test]
    fn a_pattern_gated_verb_may_not_expand_the_argument() {
        for template in ["\"{{target}}\"", "\"ping {{target}}\"", "\"{{target}}!\""] {
            let raw = format!(
                "  - name: ping\n    channel: admin\n    arg_pattern: nantes\n    get:\n      reply: {template}\n"
            );
            let err = config(&raw).unwrap_err().to_string();
            assert!(err.contains("free message text"), "{err}");
        }
        // An argument is the same leak, by a different route.
        let err = config(
            "  - name: ping\n    channel: admin\n    arg_pattern: nantes\n    get:\n      script: ok.sh\n      args: [\"{{target}}\"]\n      reply: up\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("free message text"), "{err}");

        // And the same verb without the placeholder is fine, which is what
        // makes the failure above about the placeholder rather than about the
        // combination.
        assert!(config(
            "  - name: ping\n    channel: admin\n    arg_pattern: nantes\n    get:\n      reply: up\n"
        )
        .is_ok());
    }
}
