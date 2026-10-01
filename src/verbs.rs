//! The verb table: what commands exist, which one an inbound message selected,
//! and how to answer when it selected nothing.
//!
//! A verb is a noun that doubles as a query. `garage` asks for the state,
//! `garage open` acts. That is the whole reason the language reads the way it
//! does, and it is why `help` can be rendered from this table instead of
//! written by hand — hand-written help drifts the first time a verb is added.

use std::fmt;

use crate::parse::{CONFIRM_SUFFIX, Context, Value};

/// Longest reply we will put on the air, in bytes.
///
/// MeshCore caps a channel payload at 160 bytes. `send_channel_msg` appends the
/// bytes and waits for an ACK without checking any length, so an over-long
/// reply does not truncate — it hangs on the radio. Hence a byte clamp, not a
/// character count.
pub const MAX_REPLY_BYTES: usize = 150;

/// What an action runs. A script, never a URL: the path is declared in config
/// and resolved against an allowlist, and the message can only pick which
/// declared entry fires.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    pub script: String,
    /// Argument templates, e.g. `{{target}}`. They expand only to values the
    /// verb table declares, never to free message text.
    ///
    /// An argument with no placeholder is a **literal**: it is handed to the
    /// script exactly as written. That is how a declared enum word carries the
    /// id and kind of what it names — the mapping is data in this table, not
    /// code in the script, so the same script file runs unchanged on every
    /// install. A literal is always operator-declared; the message only ever
    /// picks which declared entry fires, so a literal can never be steered.
    #[serde(default)]
    pub args: Vec<String>,
    /// Names passed through from `.env`. An explicit allowlist: the subprocess
    /// must not inherit the bot's environment, or the channel secret and every
    /// other token land in every script.
    #[serde(default)]
    pub env: Vec<String>,
    /// The action changes state. Implies a confirmation latch.
    #[serde(default)]
    pub mutating: bool,
    #[serde(default)]
    pub confirm: bool,
    /// Reply template. `{{stdout}}` is the script's clamped output.
    pub reply: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// One enum value of a verb, e.g. `open` of `garage`.
///
/// The action is nested under `action:` rather than inlined alongside `words`:
/// inlining needs `serde(flatten)`, and `flatten` silently disables
/// `deny_unknown_fields`, which is the one thing that catches a mistyped key in
/// a config that is edited by hand on a host.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgSpec {
    /// Accepted spellings, all case-insensitive. Declared here so aliases stay
    /// data and `help` can advertise them.
    pub words: Vec<String>,
    pub action: ActionSpec,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerbSpec {
    pub name: String,
    #[serde(default)]
    pub desc: String,
    /// Channel this verb answers on. `None` means the config did not scope it,
    /// which the loader rejects — the listen set and the verb scopes must agree.
    #[serde(default)]
    pub channel: Option<String>,
    /// The action for a bare `verb`, i.e. the status query.
    #[serde(default)]
    pub get: Option<ActionSpec>,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

impl VerbSpec {
    /// The first declared word, used to build the confirm prompt.
    fn primary_word(&self) -> Option<&str> {
        self.args
            .iter()
            .find_map(|a| a.words.first())
            .map(String::as_str)
    }

    fn needs_confirm(&self) -> bool {
        self.args.iter().any(|a| a.action.confirm)
    }
}

/// A verb formats as its own name, so help and confirm prompts read as
/// commands the user can actually type.
impl fmt::Display for VerbSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct VerbTable {
    verbs: Vec<VerbSpec>,
}

impl VerbTable {
    pub fn new(verbs: Vec<VerbSpec>) -> Self {
        Self { verbs }
    }

    pub fn get(&self, name: &str) -> Option<&VerbSpec> {
        let name = name.to_ascii_lowercase();
        self.verbs
            .iter()
            .find(|v| v.name.to_ascii_lowercase() == name)
    }

    /// Verb names in alphabetical order, for `help`.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.verbs.iter().map(|v| v.name.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Nearest verb within a small edit distance, so `gerage` gets a pointer
    /// instead of a flat rejection. Cheap enough at this table size to run on
    /// every miss.
    pub fn suggest(&self, word: &str) -> Option<&str> {
        let word = word.to_ascii_lowercase();
        self.verbs
            .iter()
            .map(|v| v.name.as_str())
            .map(|name| (name, edit_distance(&word, name)))
            .filter(|(name, d)| *d <= tolerance(word.len(), name.len()))
            .min_by_key(|(_, d)| *d)
            .map(|(name, _)| name)
    }

    /// Decide what a parsed message means.
    pub fn resolve<'a>(&'a self, ctx: &Context) -> Resolution<'a> {
        let Some(verb_name) = ctx.verb() else {
            return Resolution::NoVerb;
        };

        if verb_name == "help" {
            return match ctx.target() {
                Some(topic) => self.help(Some(topic)),
                None => self.help(None),
            };
        }

        let Some(verb) = self.get(verb_name) else {
            return Resolution::UnknownVerb {
                input: verb_name.to_string(),
                suggestion: self.suggest(verb_name),
            };
        };

        match ctx.target() {
            None => match &verb.get {
                Some(action) => Resolution::Fire {
                    verb,
                    action,
                    word: None,
                },
                None => Resolution::NoQuery {
                    verb,
                    suggestion: self.suggest_arg(verb, ""),
                },
            },
            Some(word) => match verb
                .args
                .iter()
                .find(|a| a.words.iter().any(|w| w.eq_ignore_ascii_case(word)))
            {
                Some(arg) => Resolution::Fire {
                    verb,
                    action: &arg.action,
                    word: Some(arg.words[0].clone()),
                },
                None => Resolution::BadArg {
                    verb,
                    input: word.to_string(),
                    suggestion: self.suggest_arg(verb, word),
                },
            },
        }
    }

    fn suggest_arg<'a>(&'a self, verb: &'a VerbSpec, word: &str) -> Option<&'a str> {
        if word.is_empty() {
            return verb.primary_word();
        }
        let word = word.to_ascii_lowercase();
        verb.args
            .iter()
            .flat_map(|a| a.words.iter())
            .map(String::as_str)
            .map(|w| (w, edit_distance(&word, &w.to_ascii_lowercase())))
            .filter(|(w, d)| *d <= tolerance(word.len(), w.len()))
            .min_by_key(|(_, d)| *d)
            .map(|(w, _)| w)
    }

    /// `help` / `help <verb>`, clamped to fit a single airtime frame.
    pub fn help(&self, topic: Option<&str>) -> Resolution<'_> {
        let Some(topic) = topic else {
            let line = format!("verbs: {} | try 'help <verb>'", self.names().join(" "));
            return Resolution::Help(clamp(&line, MAX_REPLY_BYTES));
        };

        let Some(verb) = self.get(topic) else {
            let line = match self.suggest(topic) {
                Some(name) => format!("no help for '{topic}' - did you mean: {name}?"),
                None => format!("no help for '{topic}' - try 'help'"),
            };
            return Resolution::Help(clamp(&line, MAX_REPLY_BYTES));
        };

        let mut line = format!("{}: {}", verb.name, verb.desc);
        if !verb.args.is_empty() {
            let words: Vec<&str> = verb
                .args
                .iter()
                .flat_map(|a| a.words.iter().map(String::as_str))
                .collect();
            line.push_str(" | ");
            line.push_str(&words.join(" "));
        }
        if let (true, Some(word)) = (verb.needs_confirm(), verb.primary_word()) {
            line.push_str(&format!(" | 2-step: '{verb} {word}{CONFIRM_SUFFIX}'"));
        }
        Resolution::Help(clamp(&line, MAX_REPLY_BYTES))
    }
}

/// What a message turned out to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// A reply the bot can send verbatim. Carries the whole `help` surface.
    Help(String),
    /// Run this action.
    Fire {
        verb: &'a VerbSpec,
        action: &'a ActionSpec,
        /// The declared canonical word, which is what `{{target}}` expands to —
        /// the spelling the config lists, not the one that was typed.
        word: Option<String>,
    },
    NoVerb,
    NoQuery {
        verb: &'a VerbSpec,
        suggestion: Option<&'a str>,
    },
    UnknownVerb {
        input: String,
        suggestion: Option<&'a str>,
    },
    BadArg {
        verb: &'a VerbSpec,
        input: String,
        suggestion: Option<&'a str>,
    },
}

impl<'a> Resolution<'a> {
    /// The reply to send when nothing may be run.
    pub fn into_reply(self) -> Option<String> {
        match self {
            Self::Help(line) => Some(line),
            Self::NoVerb => Some("no command - try 'help'".to_string()),
            Self::NoQuery { suggestion, .. } => Some(match suggestion {
                Some(s) => format!("try '{s}' or 'help'"),
                None => "try 'help'".to_string(),
            }),
            Self::UnknownVerb { input, suggestion } => {
                // A near-miss gets a pointer; a word that is not a near-miss gets
                // silence. The parser is verb-shaped rather than
                // sentence-shaped, so every one-word message lands here, and
                // these channels are not exclusively ours — answering "unknown:
                // 'hello' - try 'help'" to every greeting is a bot that talks
                // over itself.
                let suggestion = suggestion?;
                let line = format!("unknown: '{input}' - did you mean: {suggestion}? | try 'help'");
                Some(clamp(&line, MAX_REPLY_BYTES))
            }
            Self::BadArg {
                verb,
                input,
                suggestion,
            } => {
                let line = match suggestion {
                    Some(s) => format!("{verb}: '{input}'? did you mean: {s}?"),
                    None => format!("{verb}: '{input}'? try 'help {verb}'"),
                };
                Some(clamp(&line, MAX_REPLY_BYTES))
            }
            Self::Fire { .. } => None,
        }
    }

    /// Whether this resolution is allowed to run an action.
    ///
    /// There is deliberately no `NeedsConfirm` variant and no `gated()`: the
    /// latch in `main` is the only thing standing between a `Fire` and a spawn,
    /// and a second way to mark an action as "held" would be a path that skips
    /// it. A gated action arrives here as a plain `Fire` with `confirm: true`,
    /// and the latch decides.
    #[cfg(test)]
    pub fn is_actionable(&self) -> bool {
        matches!(self, Self::Fire { .. })
    }
}

/// The text that completes a gated command: the triggering message, then `ok`.
///
/// Echoing the input keeps the confirmation byte-identical to what was asked
/// for. Since a message is capped at [`MAX_INPUT_BYTES`] and a reply at
/// [`MAX_REPLY_BYTES`], the result is always sendable and never needs clamping.
pub fn confirm_text(input: &str) -> String {
    format!("{}{CONFIRM_SUFFIX}", input.trim())
}

/// Cut to `max` **bytes** on a character boundary, preferring to drop a whole
/// trailing word so a reply never ends mid-word.
pub fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(space) = s[..end].rfind(' ').filter(|s| *s > end / 2) {
        return s[..space].to_string();
    }
    s[..end].trim_end().to_string()
}

/// Typo tolerance scales with length: one slip in `beta`, up to three in
/// `delta`.
fn tolerance(a: usize, b: usize) -> usize {
    let longest = a.max(b);
    match longest {
        0..=4 => 1,
        5..=8 => 2,
        _ => 3,
    }
}

/// Optimal string alignment distance: Levenshtein plus adjacent transposition.
///
/// The transposition term matters because swapping two adjacent characters is
/// the most common typo on a phone keyboard, and plain Levenshtein scores
/// `opne` → `open` as 2 — outside the tolerance for a 4-character word, so
/// `garage opne` would get a flat rejection instead of the obvious suggestion.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// Build the context an engine hands to `resolve`, with the system values that
/// are not part of the message body. Kept here so tests and `main` agree.
pub fn with_system(ctx: &mut Context, channel: &str, channel_idx: u8) {
    ctx.set_system("channel", Value::Word(channel.to_string()));
    ctx.set_system("channel_idx", Value::Int(i64::from(channel_idx)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn status_action() -> ActionSpec {
        ActionSpec {
            script: "status.sh".into(),
            reply: "{{stdout}}".into(),
            timeout_secs: Some(8),
            ..ActionSpec::default()
        }
    }

    fn mutating_action(confirm: bool) -> ActionSpec {
        ActionSpec {
            script: "pve-reboot.sh".into(),
            args: vec!["{{target}}".into()],
            env: vec!["PVE_NODE".into()],
            mutating: true,
            confirm,
            reply: "{{stdout}}".into(),
            timeout_secs: Some(10),
        }
    }

    fn table() -> VerbTable {
        VerbTable::new(vec![
            VerbSpec {
                name: "alarm".into(),
                desc: "alarm state".into(),
                channel: Some("#homeassistant".into()),
                get: Some(status_action()),
                args: vec![
                    ArgSpec {
                        words: vec!["arm".into(), "on".into(), "lock".into()],
                        action: mutating_action(false),
                    },
                    ArgSpec {
                        words: vec!["disarm".into(), "off".into()],
                        action: mutating_action(false),
                    },
                ],
            },
            VerbSpec {
                name: "garage".into(),
                desc: "garage door".into(),
                channel: Some("#homeassistant".into()),
                get: Some(status_action()),
                args: vec![ArgSpec {
                    words: vec!["open".into(), "up".into()],
                    action: mutating_action(false),
                }],
            },
            VerbSpec {
                name: "internet".into(),
                desc: "wan link".into(),
                channel: Some("#admin".into()),
                get: Some(status_action()),
                args: vec![],
            },
            VerbSpec {
                name: "reboot".into(),
                desc: "reboot a host".into(),
                channel: Some("#admin".into()),
                get: None,
                args: vec![ArgSpec {
                    words: vec![
                        "alpha".into(),
                        "beta".into(),
                        "gamma".into(),
                        "delta".into(),
                        "komodo".into(),
                        "pve".into(),
                    ],
                    action: mutating_action(true),
                }],
            },
        ])
    }

    fn resolve(input: &str) -> Resolution<'_> {
        // The table has to outlive the resolution, which borrows from it.
        static TABLE: std::sync::OnceLock<VerbTable> = std::sync::OnceLock::new();
        let table = TABLE.get_or_init(table);
        let ctx = parse::parse(input).unwrap();
        table.resolve(&ctx)
    }

    #[test]
    fn bare_verb_fires_the_status_action() {
        match resolve("garage") {
            Resolution::Fire { verb, word, .. } => {
                assert_eq!(verb.name, "garage");
                assert_eq!(word, None);
            }
            other => panic!("expected Fire, got {other:?}"),
        }
    }

    #[test]
    fn a_word_selects_the_declared_arg() {
        match resolve("garage up") {
            Resolution::Fire { word, .. } => assert_eq!(word.as_deref(), Some("open")),
            other => panic!("expected Fire, got {other:?}"),
        }
    }

    #[test]
    fn aliases_resolve_to_the_canonical_declared_word() {
        // `alarm on` is accepted, but {{target}} expands to the config's own
        // spelling so the mapping in the script is the only one that matters.
        match resolve("alarm on") {
            Resolution::Fire { word, .. } => assert_eq!(word.as_deref(), Some("arm")),
            other => panic!("expected Fire, got {other:?}"),
        }
    }

    /// Only `reboot` sets `confirm`, which is what arms the latch. Everything
    /// else is either a read or an idempotent door/alarm action that a second
    /// message would not improve.
    #[test]
    fn only_reboot_asks_for_confirmation() {
        let table = &crate::config::example().table;
        for verb in &table.verbs {
            for arg in &verb.args {
                assert_eq!(
                    arg.action.confirm,
                    verb.name == "reboot",
                    "{} {}",
                    verb.name,
                    arg.words[0]
                );
            }
        }
    }

    /// The dry-run guard has to be *visible to the script* to be armable. The
    /// executor empties the child's environment, so a variable the verb entry
    /// omits never reaches the script — which would pin `PVE_ALLOW_REBOOT` off
    /// and leave `reboot` permanently inert, with nothing in the logs to say why.
    ///
    /// This looks like an odd thing to assert about a config table, so it is
    /// worth saying plainly: it is a guard against a silent, permanent no-op on
    /// the only destructive action in the service.
    #[test]
    fn the_reboot_guard_is_in_the_env_allowlist() {
        let table = &crate::config::example().table;
        let reboot = table.get("reboot").expect("reboot is declared");
        for arg in &reboot.args {
            assert!(
                arg.action.env.contains(&"PVE_ALLOW_REBOOT".to_string()),
                "pve-reboot.sh can never be armed without {} declared",
                "PVE_ALLOW_REBOOT"
            );
        }
    }

    /// Every action names the script it runs, and no argument looks like a path.
    ///
    /// A literal argument is expected now — a declared word carries the id and kind
    /// of what it names — and a literal is indistinguishable from an expanded slot
    /// once the string exists, so parsing cannot tell the two apart and neither can
    /// this test. What survives is the check that matters either way: no argument
    /// contains a path separator or a quote, so nothing an action is handed could
    /// name a file or open a quoted string if a script ever re-parsed its argv.
    #[test]
    fn no_action_argument_carries_a_path_or_a_quote() {
        let table = &crate::config::example().table;
        for verb in &table.verbs {
            let actions = verb.get.iter().chain(verb.args.iter().map(|a| &a.action));
            for action in actions {
                assert!(!action.script.is_empty(), "{} has no script", verb.name);
                assert!(
                    !action.script.contains('/'),
                    "{} script must be a bare name",
                    verb.name
                );
                for arg in &action.args {
                    assert!(
                        !arg.contains('/') && !arg.contains('"'),
                        "{}: argument {arg:?} looks like a supplied path",
                        verb.name
                    );
                }
            }
        }
    }

    /// `reboot` hands the script everything the script cannot know: which machine
    /// the word means, and which token that machine needs.
    ///
    /// The two halves are separate failures. Passing the guest token where the host
    /// one belongs is a 401 at best and an unhelpful one at worst; passing a literal
    /// where `{{target}}` belongs would silently stop echoing the machine's name on
    /// the air. So both are pinned here rather than left to review.
    #[test]
    fn reboot_declares_the_id_kind_and_token_of_each_word() {
        let table = &crate::config::example().table;
        let reboot = table.get("reboot").expect("reboot is declared");

        for arg in &reboot.args {
            let word = &arg.words[0];
            let [target, vmid, kind] = arg.action.args.as_slice() else {
                panic!("{word}: reboot takes <word> <vmid> <kind>");
            };
            assert_eq!(target, "{{target}}", "{word}: arg 0 must be the word");
            assert!(
                matches!(kind.as_str(), "qemu" | "lxc" | "host"),
                "{word}: unknown kind {kind:?}"
            );
            match kind.as_str() {
                "host" => assert_eq!(vmid, "-", "{word}: the node endpoint has no vmid"),
                _ => assert!(
                    vmid.chars().all(|c| c.is_ascii_digit()) && vmid.len() >= 3,
                    "{word}: guest id must be numeric"
                ),
            }

            let tokens: Vec<&String> = arg
                .action
                .env
                .iter()
                .filter(|name| name.starts_with("PVE_TOKEN"))
                .collect();
            assert_eq!(tokens.len(), 1, "{word}: exactly one token");
            let expected = if kind == "host" {
                "PVE_TOKEN_HOST"
            } else {
                "PVE_TOKEN_GUEST"
            };
            assert_eq!(
                tokens[0].as_str(),
                expected,
                "{word}: a {kind} must use {expected}"
            );
        }
    }

    /// The confirm prompt must echo what was typed. Using the canonical word
    /// here would answer `reboot delta` with `reboot alpha ok`, and the
    /// confirmation would then reboot a different machine.
    #[test]
    fn the_confirm_text_echoes_the_typed_command() {
        for input in ["reboot delta", "REBOOT  delta", "reboot delta "] {
            assert_eq!(confirm_text(input), format!("{} ok", input.trim()));
        }
    }

    /// The two spellings share a latch, so a confirmation is accepted for
    /// `reboot alpha` even when the armed message was typed differently.
    #[test]
    fn equivalent_spellings_share_one_canonical_key() {
        assert_eq!(
            parse::parse("reboot alpha").unwrap().canonical(),
            parse::parse("REBOOT  alpha").unwrap().canonical()
        );
    }

    #[test]
    fn a_verb_without_a_query_explains_itself() {
        match resolve("reboot") {
            Resolution::NoQuery { suggestion, .. } => assert_eq!(suggestion, Some("alpha")),
            other => panic!("expected NoQuery, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_verb_is_answered_never_ignored() {
        let reply = resolve("gerage").into_reply().unwrap();
        assert_eq!(
            reply,
            "unknown: 'gerage' - did you mean: garage? | try 'help'"
        );
        assert!(!resolve("gerage").is_actionable());
    }

    #[test]
    fn a_bad_arg_suggests_the_nearest_word() {
        let reply = resolve("garage opne").into_reply().unwrap();
        assert_eq!(reply, "garage: 'opne'? did you mean: open?");
    }

    #[test]
    fn help_lists_every_verb_and_fits_one_frame() {
        let reply = resolve("help").into_reply().unwrap();
        assert_eq!(
            reply,
            "verbs: alarm garage internet reboot | try 'help <verb>'"
        );
        assert!(reply.len() <= MAX_REPLY_BYTES);
    }

    #[test]
    fn help_for_a_verb_shows_words_and_the_confirm_prompt() {
        let reply = resolve("help reboot").into_reply().unwrap();
        assert_eq!(
            reply,
            "reboot: reboot a host | alpha beta gamma delta komodo pve | \
             2-step: 'reboot alpha ok'"
        );
        assert!(reply.len() <= MAX_REPLY_BYTES);
    }

    #[test]
    fn help_for_a_query_only_verb_omits_the_word_list() {
        assert_eq!(
            resolve("help internet").into_reply().unwrap(),
            "internet: wan link"
        );
    }

    #[test]
    fn help_suggests_on_a_near_miss_topic() {
        assert_eq!(
            resolve("help rebooot").into_reply().unwrap(),
            "no help for 'rebooot' - did you mean: reboot?"
        );
    }

    #[test]
    fn help_never_actions_anything() {
        assert!(!resolve("help").is_actionable());
        assert!(!resolve("help reboot").is_actionable());
    }

    #[test]
    fn clamp_cuts_on_a_character_boundary() {
        assert_eq!(clamp("hello", 10), "hello");
        assert_eq!(clamp("hello", 3), "hel");
        // Multi-byte: "é" is two bytes, so a 2-byte cap cannot split it.
        assert_eq!(clamp("éé", 1), "");
        assert_eq!(clamp("éé", 2), "é");
    }

    #[test]
    fn clamp_drops_a_whole_trailing_word_when_it_can() {
        assert_eq!(clamp("one two three", 9), "one two");
    }

    #[test]
    fn every_help_line_fits_the_airtime_budget() {
        for topic in ["alarm", "garage", "internet", "reboot", "help"] {
            let reply = resolve(&format!("help {topic}")).into_reply().unwrap();
            assert!(
                reply.len() <= MAX_REPLY_BYTES,
                "help {topic} is {} bytes",
                reply.len()
            );
        }
    }

    /// The declared scripts are the contract with `scripts.example/`. If a verb
    /// names a script no template exists for, the action fails at the allowlist
    /// check and the bot looks broken on a real radio.
    #[test]
    fn every_declared_script_has_a_template() {
        const TEMPLATES: &[&str] = &[
            "ha-entity.sh",
            "ha-service.sh",
            "internet-status.sh",
            "komodo-status.sh",
            "pve-reboot.sh",
        ];

        let table = &crate::config::example().table;
        for name in table.names() {
            let verb = table.get(name).unwrap();
            let actions = verb.get.iter().chain(verb.args.iter().map(|a| &a.action));
            for action in actions {
                assert!(
                    TEMPLATES.contains(&action.script.as_str()),
                    "{name} runs {:?}, which has no template",
                    action.script
                );
            }
        }
    }

    /// Anything that asks for the latch must be marked as a state change, or a
    /// later `mutating: false` typo would quietly drop the confirmation.
    #[test]
    fn confirm_implies_mutating() {
        let table = &crate::config::example().table;
        for name in table.names() {
            let verb = table.get(name).unwrap();
            for arg in &verb.args {
                assert!(
                    !arg.action.confirm || arg.action.mutating,
                    "{name} confirms without being mutating"
                );
            }
        }
    }

    /// A script that needs a token must be told where to find it. The executor
    /// clears the environment before spawning, so a missing `env` entry is an
    /// empty environment, not an inherited one.
    #[test]
    fn every_script_that_needs_a_secret_declares_its_env() {
        let table = &crate::config::example().table;
        for name in table.names() {
            let verb = table.get(name).unwrap();
            let actions = verb.get.iter().chain(verb.args.iter().map(|a| &a.action));
            for action in actions {
                let needs = action.script.starts_with("ha-")
                    || action.script.starts_with("komodo")
                    || action.script.starts_with("pve-");
                assert!(
                    !needs || !action.env.is_empty(),
                    "{name} runs {} with no env allowlist",
                    action.script
                );
            }
        }
    }

    #[test]
    fn the_default_verb_set_resolves_and_fits_the_budget() {
        let table = &crate::config::example().table;
        for name in table.names() {
            // A bare verb is actionable only where the config declares a status
            // query; `reboot` has none, and `help` is handled before the table.
            let verb = table.get(name).unwrap();
            let resolution = table.resolve(&parse::parse(name).unwrap());
            assert_eq!(
                resolution.is_actionable(),
                verb.get.is_some(),
                "{name} actionability disagrees with its declared query"
            );

            for word in verb.args.iter().flat_map(|a| a.words.iter()) {
                let ctx = parse::parse(&format!("{name} {word}")).unwrap();
                assert!(
                    table.resolve(&ctx).is_actionable(),
                    "{name} {word} does not resolve"
                );
            }

            let ctx = parse::parse(&format!("help {name}")).unwrap();
            let reply = table.resolve(&ctx).into_reply().unwrap();
            assert!(reply.len() <= MAX_REPLY_BYTES, "help {name}: {reply}");
        }
    }
}
