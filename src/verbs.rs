//! The verb table: what commands exist, which one an inbound message selected,
//! and how to answer when it selected nothing.
//!
//! A verb is a noun that doubles as a query. `garage` asks for the state,
//! `garage open` acts. That is the whole reason the language reads the way it
//! does, and it is why `help` can be rendered from this table instead of
//! written by hand — hand-written help drifts the first time a verb is added.

use std::fmt;

use crate::parse::{CONFIRM_SUFFIX, Context, MessageMeta, Value};

/// Longest reply we will put on the air, in bytes.
///
/// MeshCore caps a channel payload at 160 bytes. `send_channel_msg` appends the
/// bytes and waits for an ACK without checking any length, so an over-long
/// reply does not truncate — it hangs on the radio. Hence a byte clamp, not a
/// character count.
pub const MAX_REPLY_BYTES: usize = 150;

/// How many messages one action's reply may become.
///
/// A script that reports several things — every unhealthy machine, say — puts one
/// on each line of stdout, and each line is a separate transmission. Airtime on
/// LoRa is shared with everything else on the channel, so the count is bounded
/// rather than left to the script. The overflow is summarised instead of dropped:
/// a truncated list that silently loses four entries is worse than a shorter list
/// that says it is shorter.
pub const MAX_REPLIES: usize = 4;

/// What an action runs. A script, never a URL: the path is declared in config
/// and resolved against an allowlist, and the message can only pick which
/// declared entry fires.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    /// The script to run, or `None` for a **reply-only** action: one whose reply
    /// is built entirely from the message context (for example `ping`, which
    /// reports hops and delay) and needs no external process. A reply-only
    /// action must not use `{{stdout}}`, which the loader enforces.
    #[serde(default)]
    pub script: Option<String>,
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

/// One reply per line of a rendered template, clamped and capped.
///
/// A single line still produces a single reply, so every existing action behaves
/// exactly as before: the templates in `scripts.example/` all end in one line and
/// this function returns one message for them.
///
/// A **single** line that is too long is not truncated: it is split into
/// numbered fragments by [`split_numbered`], so a long repeater chain arrives
/// whole as `1/2 ...`, `2/2 ...`. Multi-line output is already one message per
/// line, so those keep the per-line clamp and the overall cap.
///
/// Empty lines are dropped rather than sent, which preserves the rule that an
/// action rendering to nothing says nothing instead of putting a blank frame on
/// the air. Each line is clamped on its own, so a long first line cannot eat the
/// budget of the ones after it.
pub fn split_reply(rendered: &str) -> Vec<String> {
    let lines: Vec<&str> = rendered
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    if lines.len() == 1 {
        return split_numbered(lines[0], MAX_REPLY_BYTES);
    }

    let lines: Vec<String> = lines
        .into_iter()
        .map(|line| clamp(line, MAX_REPLY_BYTES))
        .collect();

    if lines.len() <= MAX_REPLIES {
        return lines;
    }

    let omitted = lines.len() - MAX_REPLIES;
    let mut out = lines[..MAX_REPLIES].to_vec();
    out.push(format!("+{omitted} more"));
    out
}

/// Split one over-long message into `i/n` fragments that each fit `max` bytes.
///
/// Word boundaries are preferred so a repeater name is never cut in half; a
/// single token longer than a whole fragment is the one case that is cut
/// mid-token, on a character boundary. The `i/n ` prefix counts against the
/// budget, and a little more is reserved for the truncation suffix, so every
/// fragment that comes back is sendable as-is.
///
/// At most [`MAX_REPLIES`] fragments are sent. If the text needs more, the last
/// fragment is trimmed to make room for a `…+N more` marker rather than dropping
/// the rest silently.
pub fn split_numbered(text: &str, max: usize) -> Vec<String> {
    if text.len() <= max {
        return vec![text.to_string()];
    }

    // Room for the widest `i/n ` prefix we could need, plus a truncation suffix.
    // Reserving both up front means the packing pass never has to be redone and
    // no fragment is ever handed to `send` already over budget.
    const PREFIX_RESERVE: usize = 8;
    const SUFFIX_RESERVE: usize = 16;
    let budget = max.saturating_sub(PREFIX_RESERVE + SUFFIX_RESERVE).max(1);

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let sep = usize::from(!current.is_empty());
            if current.len() + sep + word.len() <= budget {
                if sep == 1 {
                    current.push(' ');
                }
                current.push_str(word);
                break;
            }
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                continue;
            }
            // The word alone is wider than a fragment: cut a head off it.
            let head = clamp(word, budget);
            if head.is_empty() {
                // Budget smaller than one character. Take the rest so the loop
                // cannot spin.
                current.push_str(word);
                break;
            }
            let head_len = head.len();
            chunks.push(head);
            word = &word[head_len..];
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    let total = chunks.len();
    let sent = total.min(MAX_REPLIES);
    chunks.truncate(sent);
    let mut out: Vec<String> = chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| format!("{}/{sent} {chunk}", i + 1))
        .collect();
    if total > sent
        && let Some(last) = out.last_mut()
    {
        push_within(last, &format!(" …+{} more", total - sent), max);
    }
    out
}

/// Append `suffix` to `s` without letting the result exceed `max` bytes.
///
/// Used for the truncation marker on the last fragment of a split: the fragment
/// was packed to leave room, but a defensive trim here means the invariant holds
/// even if the reserve above ever changes.
fn push_within(s: &mut String, suffix: &str, max: usize) {
    if s.len() + suffix.len() <= max {
        s.push_str(suffix);
        return;
    }
    let budget = max.saturating_sub(suffix.len());
    let cut = clamp(s, budget);
    let cut_len = cut.len();
    s.truncate(cut_len);
    s.push_str(suffix);
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
///
/// The message metadata is injected here rather than parsed, so a template can
/// read `{{hops}}`, `{{delay}}`, `{{snr}}`, `{{sender_timestamp}}` and
/// `{{repeaters}}` without any of them being message-supplied. `{{sender}}` is
/// injected by `decide` instead, because it comes from the raw text's tag.
pub fn with_system(ctx: &mut Context, channel: &str, channel_idx: u8, meta: &MessageMeta) {
    ctx.set_system("channel", Value::Word(channel.to_string()));
    ctx.set_system("channel_idx", Value::Int(i64::from(channel_idx)));
    ctx.set_system("hops", Value::Int(i64::from(meta.path_len)));
    // A sender whose clock runs ahead would otherwise produce a negative delay,
    // which reads as nonsense on the air.
    ctx.set_system(
        "delay",
        Value::Int(i64::from(meta.now.saturating_sub(meta.sender_timestamp))),
    );
    ctx.set_system(
        "sender_timestamp",
        Value::Int(i64::from(meta.sender_timestamp)),
    );
    if let Some(snr) = meta.snr {
        ctx.set_system("snr", Value::Float(f64::from(snr)));
    }
    ctx.set_system("repeaters", Value::Str(meta.repeaters.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn status_action() -> ActionSpec {
        ActionSpec {
            script: Some("status.sh".into()),
            reply: "{{stdout}}".into(),
            timeout_secs: Some(8),
            ..ActionSpec::default()
        }
    }

    fn mutating_action(confirm: bool) -> ActionSpec {
        ActionSpec {
            script: Some("pve-reboot.sh".into()),
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
                if let Some(script) = &action.script {
                    assert!(!script.is_empty(), "{} has no script", verb.name);
                    assert!(
                        !script.contains('/'),
                        "{} script must be a bare name",
                        verb.name
                    );
                }
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
    fn a_single_line_still_produces_a_single_reply() {
        assert_eq!(split_reply("all ok"), ["all ok"]);
        // The trailing newline every `printf` leaves is not a second message.
        assert_eq!(split_reply("armed\n"), ["armed"]);
    }

    #[test]
    fn each_line_becomes_its_own_reply() {
        assert_eq!(
            split_reply("1/3 server a not ok\n2/3 stack b unhealthy\n3/3 stack c down"),
            [
                "1/3 server a not ok",
                "2/3 stack b unhealthy",
                "3/3 stack c down"
            ]
        );
    }

    /// Blank lines are dropped rather than sent: an action that renders to
    /// nothing says nothing, instead of putting an empty frame on the air.
    #[test]
    fn empty_and_blank_input_produces_no_reply() {
        assert!(split_reply("").is_empty());
        assert!(split_reply("\n\n  \n").is_empty());
        assert!(split_reply("real\n\n").len() == 1);
    }

    /// Each line gets its own budget, so a long first line cannot consume the
    /// allowance of the ones after it.
    #[test]
    fn each_line_is_clamped_independently() {
        let replies = split_reply(&format!("{}\nshort", "x".repeat(400)));
        assert_eq!(replies.len(), 2, "{replies:?}");
        assert!(replies[0].len() <= MAX_REPLY_BYTES);
        assert_eq!(replies[1], "short");
    }

    /// Airtime is shared with everything else on the channel, so the count is
    /// bounded — and the overflow is stated rather than dropped, because a list
    /// that silently loses four entries is worse than one that says it is short.
    #[test]
    fn a_long_list_is_capped_with_a_summary() {
        let rendered = (1..=9)
            .map(|i| format!("{i}/9 stack s{i} unhealthy"))
            .collect::<Vec<_>>()
            .join("\n");
        let replies = split_reply(&rendered);

        assert_eq!(replies.len(), MAX_REPLIES + 1);
        assert_eq!(replies[..MAX_REPLIES].len(), 4);
        // The script's own denominators stay honest: the cap hides messages, not
        // facts, so what is shown still says it came out of nine.
        assert_eq!(replies[3], "4/9 stack s4 unhealthy");
        assert_eq!(replies[4], "+5 more");
    }

    #[test]
    fn exactly_the_cap_is_not_a_summary() {
        let rendered = (1..=MAX_REPLIES)
            .map(|i| format!("{i}/4 stack s{i} unhealthy"))
            .collect::<Vec<_>>()
            .join("\n");
        let replies = split_reply(&rendered);

        assert_eq!(replies.len(), MAX_REPLIES);
        assert_eq!(replies[3], "4/4 stack s4 unhealthy");
    }

    // ---- numbered splitting of one over-long message

    #[test]
    fn a_short_message_is_not_split() {
        assert_eq!(
            split_numbered("ping: 1s, 0 hops | ?", MAX_REPLY_BYTES),
            ["ping: 1s, 0 hops | ?"]
        );
    }

    #[test]
    fn an_over_long_message_splits_into_numbered_fragments() {
        let chain = (0..40)
            .map(|i| format!("NODE-{i:02}"))
            .collect::<Vec<_>>()
            .join(" > ");
        let text = format!("ping: 12s, 40 hops | {chain}");
        assert!(text.len() > MAX_REPLY_BYTES);

        let parts = split_numbered(&text, MAX_REPLY_BYTES);
        assert!(parts.len() >= 2, "{parts:?}");
        for (i, part) in parts.iter().enumerate() {
            assert!(
                part.len() <= MAX_REPLY_BYTES,
                "{part:?} is {} bytes",
                part.len()
            );
            assert!(
                part.starts_with(&format!("{}/{} ", i + 1, parts.len())),
                "{part:?}"
            );
        }

        // No word was cut in half: the payloads rejoin to the original tokens.
        let payload: Vec<&str> = parts
            .iter()
            .map(|p| p.split_once(' ').map(|(_, rest)| rest).unwrap_or(""))
            .flat_map(str::split_whitespace)
            .collect();
        assert_eq!(
            payload,
            text.split_whitespace().collect::<Vec<_>>(),
            "a fragment dropped or mangled a word"
        );
    }

    #[test]
    fn a_single_token_wider_than_a_fragment_is_cut() {
        let token = "A".repeat(MAX_REPLY_BYTES * 2);
        let parts = split_numbered(&token, MAX_REPLY_BYTES);
        assert!(parts.len() >= 2, "{parts:?}");
        for part in &parts {
            assert!(part.len() <= MAX_REPLY_BYTES, "{} bytes", part.len());
        }
        let payload: String = parts
            .iter()
            .map(|p| p.split_once(' ').map(|(_, rest)| rest).unwrap_or(""))
            .collect();
        assert_eq!(payload, token, "the token was not reassembled exactly");
    }

    #[test]
    fn a_very_long_message_is_capped_with_a_truncation_marker() {
        let text = (0..400)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let parts = split_numbered(&text, MAX_REPLY_BYTES);
        assert_eq!(parts.len(), MAX_REPLIES, "{parts:?}");
        for part in &parts {
            assert!(
                part.len() <= MAX_REPLY_BYTES,
                "{} bytes: {part}",
                part.len()
            );
        }
        assert!(
            parts.last().unwrap().contains("more"),
            "no truncation marker: {:?}",
            parts.last()
        );
    }

    #[test]
    fn an_over_long_single_reply_is_split_not_truncated() {
        let text = (0..40)
            .map(|i| format!("NODE-{i:02}"))
            .collect::<Vec<_>>()
            .join(" > ");
        let parts = split_reply(&text);
        assert!(parts.len() >= 2, "{parts:?}");
        assert!(parts[0].starts_with("1/"), "{:?}", parts[0]);
    }

    // ---- injected message metadata

    #[test]
    fn with_system_injects_the_message_metadata() {
        let mut ctx = parse::parse("ping").unwrap();
        let meta = crate::parse::MessageMeta {
            path_len: 3,
            snr: Some(7.5),
            sender_timestamp: 1000,
            repeaters: "A > B > C".into(),
            now: 1012,
        };
        with_system(&mut ctx, "#admin", 3, &meta);

        assert_eq!(crate::script::expand("{{hops}}", &ctx, None).unwrap(), "3");
        assert_eq!(
            crate::script::expand("{{delay}}", &ctx, None).unwrap(),
            "12"
        );
        assert_eq!(
            crate::script::expand("{{repeaters}}", &ctx, None).unwrap(),
            "A > B > C"
        );
        assert_eq!(crate::script::expand("{{snr}}", &ctx, None).unwrap(), "7.5");
        assert_eq!(
            crate::script::expand("{{sender_timestamp}}", &ctx, None).unwrap(),
            "1000"
        );
    }

    /// A sender whose clock is ahead must read as `0s`, not a negative delay.
    #[test]
    fn a_sender_ahead_of_us_yields_zero_delay() {
        let mut ctx = parse::parse("ping").unwrap();
        let meta = crate::parse::MessageMeta {
            sender_timestamp: 2000,
            now: 1000,
            ..crate::parse::MessageMeta::default()
        };
        with_system(&mut ctx, "#admin", 3, &meta);
        assert_eq!(crate::script::expand("{{delay}}", &ctx, None).unwrap(), "0");
    }

    /// The metadata is reserved, so a relayed message cannot pre-empt it; and
    /// even if it could, `with_system` runs last and overwrites.
    #[test]
    fn a_message_cannot_spoof_the_metadata() {
        let mut ctx = parse::parse("ping hops=99 delay=1 repeaters=x").unwrap();
        assert!(ctx.is_poisoned("hops"));
        assert!(ctx.is_poisoned("delay"));
        assert!(ctx.is_poisoned("repeaters"));

        let meta = crate::parse::MessageMeta {
            path_len: 2,
            now: 100,
            ..crate::parse::MessageMeta::default()
        };
        with_system(&mut ctx, "#admin", 3, &meta);
        assert_eq!(crate::script::expand("{{hops}}", &ctx, None).unwrap(), "2");
        assert_eq!(
            crate::script::expand("{{delay}}", &ctx, None).unwrap(),
            "100"
        );
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
                if let Some(script) = &action.script {
                    assert!(
                        TEMPLATES.contains(&script.as_str()),
                        "{name} runs {script:?}, which has no template"
                    );
                }
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
                let needs = action.script.as_deref().is_some_and(|s| {
                    s.starts_with("ha-") || s.starts_with("komodo") || s.starts_with("pve-")
                });
                assert!(
                    !needs || !action.env.is_empty(),
                    "{name} runs {:?} with no env allowlist",
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
