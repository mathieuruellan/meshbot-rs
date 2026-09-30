//! Turning an inbound channel message into a command context.
//!
//! A context is a flat key/value map plus the verb. It is deliberately not a
//! struct: rules and templates address it by name, and a flat map keeps the
//! reserved-key rule checkable in one place.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use pest::Parser;
use pest_derive::Parser;

/// Longest accepted input, in **bytes**. LoRa caps a channel payload at 160
/// bytes, so 64 leaves comfortable headroom — and no command in the language
/// comes close, the longest being `reboot alpha ok` at 15.
pub const MAX_INPUT_BYTES: usize = 64;

/// Suffix appended to a command that armed a confirmation latch.
pub const CONFIRM_SUFFIX: &str = " ok";

/// The same token as a bare word, which is how it appears once a user types it.
const CONFIRM_WORD: &str = "ok";

/// Keys a message body may never set, whatever it asks for.
///
/// `channel` and `channel_idx` are the ones that matter: a relayed message
/// carrying `channel=#admin` must not be able to make a rule written for
/// the admin channel match on the public one. The rest are values the engine
/// injects after parsing, so a message cannot pre-empt them either.
///
/// `confirm` is security-critical rather than cosmetic. Without it reserved,
/// `reboot alpha confirm=true` is an ordinary slot and the latch is skipped
/// entirely, so the one gate standing between a mesh message and a reboot can
/// be switched off by the message it is gating.
pub const RESERVED_KEYS: &[&str] = &[
    "verb",
    "confirm",
    "channel",
    "channel_idx",
    "snr",
    "sender_timestamp",
    "status",
    "stdout",
    "target",
];

#[derive(Parser)]
#[grammar = "meshbot.pest"]
struct Grammar;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Word(String),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Bool(v) => write!(f, "{v}"),
            Self::Str(v) | Self::Word(v) => f.write_str(v),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    TooLong { len: usize, max: usize },
    IllegalChar(char),
    NoVerb,
    DuplicateKey(String),
    Syntax(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { len, max } => write!(f, "input is {len} bytes, limit is {max}"),
            Self::IllegalChar(c) => write!(f, "illegal character {c:?} in input"),
            Self::NoVerb => f.write_str("no verb"),
            Self::DuplicateKey(k) => write!(f, "duplicate key {k:?}"),
            Self::Syntax(m) => write!(f, "syntax error: {m}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Extra keys a message may not set, on top of the engine's own.
///
/// The `.env` names go here because a declared action interpolates them into
/// templates and hands them to a script. A message that could set `HA_TOKEN` as
/// a slot would have that value written into a reply, and a `{{HA_TOKEN}}`
/// template would pick it up in place of the real one.
#[derive(Debug, Clone, Default)]
pub struct Reserved(BTreeSet<String>);

impl Reserved {
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self(
            names
                .into_iter()
                .map(|n| n.as_ref().trim().to_ascii_lowercase())
                .filter(|n| !n.is_empty())
                .collect(),
        )
    }
}

/// The parsed form of one message.
#[derive(Debug, Clone, Default)]
pub struct Context {
    values: BTreeMap<String, Value>,
    reserved: BTreeSet<String>,
    /// Message-supplied keys that collided with a reserved key. The real value
    /// survives untouched, and any rule that consults a poisoned key is treated
    /// as not matching.
    poisoned: BTreeSet<String>,
}

impl Context {
    pub fn new_with(extra: &Reserved) -> Self {
        Self {
            values: BTreeMap::new(),
            reserved: RESERVED_KEYS
                .iter()
                .map(|k| (*k).to_string())
                .chain(extra.0.iter().cloned())
                .collect(),
            poisoned: BTreeSet::new(),
        }
    }

    pub fn verb(&self) -> Option<&str> {
        match self.values.get("verb") {
            Some(Value::Word(w)) | Some(Value::Str(w)) => Some(w),
            _ => None,
        }
    }

    /// The enum word that selected the action, e.g. `open` in `garage open`.
    pub fn target(&self) -> Option<&str> {
        match self.values.get("target") {
            Some(Value::Word(w)) | Some(Value::Str(w)) => Some(w),
            _ => None,
        }
    }

    /// Whether the message carried the confirmation token, e.g. `ok` in
    /// `reboot alpha ok`.
    // Dead until the latch consults it.
    #[allow(dead_code)]
    pub fn confirmed(&self) -> bool {
        self.values
            .get("confirm")
            .is_some_and(|v| v.to_string().eq_ignore_ascii_case(CONFIRM_WORD))
    }

    // Dead until `{{slot}}` interpolation in actions reads it.
    #[allow(dead_code)]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// True when a message tried to set this key and was refused. A rule that
    /// tests such a key must not match, even though the real value is present.
    pub fn is_poisoned(&self, key: &str) -> bool {
        self.poisoned.contains(key)
    }

    pub fn poisoned_keys(&self) -> impl Iterator<Item = &str> {
        self.poisoned.iter().map(String::as_str)
    }

    /// Set a key the engine owns. Overwrites anything already there.
    pub fn set_system(&mut self, key: &str, value: Value) {
        self.values.insert(key.to_ascii_lowercase(), value);
    }

    /// Set a key from the message body.
    ///
    /// A reserved key is never overwritten: the engine's value stands and the
    /// key is marked poisoned, which is what stops a relayed message from
    /// spoofing the channel a rule is scoped to.
    pub fn set_message(&mut self, key: &str, value: Value) {
        let key = key.to_ascii_lowercase();
        if self.reserved.contains(&key) {
            self.poisoned.insert(key);
            return;
        }
        self.values.insert(key, value);
    }

    /// The message body re-rendered canonically: verb, then the enum word, then
    /// its slots in sorted order. Used to key the confirmation latch, so
    /// `reboot alpha` and `REBOOT  alpha` share one armed state — and so
    /// `reboot alpha ok` keys the same entry, which is why `confirm` is
    /// filtered out along with the rest.
    ///
    /// Every reserved key is excluded, not just `verb` and `target`. The engine
    /// injects `channel` and `channel_idx` after parsing, and including them
    /// would key the latch on `reboot alpha channel=#admin channel_idx=3`.
    // Dead until the latch keys on it.
    #[allow(dead_code)]
    pub fn canonical(&self) -> String {
        let mut out = String::from(self.verb().unwrap_or_default());
        if let Some(target) = self.target() {
            out.push(' ');
            out.push_str(target);
        }
        let mut slots: Vec<String> = self
            .values
            .iter()
            .filter(|(k, _)| !RESERVED_KEYS.contains(&k.as_str()))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        slots.sort();
        for slot in slots {
            out.push(' ');
            out.push_str(&slot);
        }
        out
    }
}

/// Parse with no extra reserved keys. Test shorthand for [`parse_with`].
#[cfg(test)]
pub fn parse(input: &str) -> Result<Context, ParseError> {
    parse_with(input, &Reserved::default())
}

pub fn parse_with(input: &str, extra: &Reserved) -> Result<Context, ParseError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(ParseError::TooLong {
            len: input.len(),
            max: MAX_INPUT_BYTES,
        });
    }
    if let Some(c) = input.chars().find(|c| matches!(c, '\n' | '\r' | '\t')) {
        return Err(ParseError::IllegalChar(c));
    }

    let mut pairs = Grammar::parse(Rule::command, input)
        .map_err(|e| ParseError::Syntax(first_line(&e.to_string())))?;
    let command = pairs.next().ok_or(ParseError::NoVerb)?;

    let mut ctx = Context::new_with(extra);
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for pair in command.into_inner() {
        match pair.as_rule() {
            Rule::verb => {
                let raw = pair.as_str().to_ascii_lowercase();
                ctx.set_system("verb", Value::Word(raw));
            }
            Rule::arg => {
                let arg = pair.into_inner().next().ok_or(ParseError::NoVerb)?;
                match arg.as_rule() {
                    Rule::slot => {
                        let mut parts = arg.into_inner();
                        let key = parts
                            .next()
                            .ok_or(ParseError::NoVerb)?
                            .as_str()
                            .to_ascii_lowercase();
                        let value = decode(parts.next().ok_or(ParseError::NoVerb)?)?;
                        if !seen.insert(key.clone()) {
                            return Err(ParseError::DuplicateKey(key));
                        }
                        ctx.set_message(&key, value);
                    }
                    // The grammar guarantees the verb precedes every arg, so the
                    // first bare word is unambiguously the enum value selecting
                    // an action. It is a structural part of the command rather
                    // than a free-form slot, so it is set as a system value —
                    // which also means a literal `target=x` in the body is still
                    // refused and poisoned by `set_message`.
                    //
                    // The second bare word is the confirmation token. It has to
                    // be recognised here rather than left to the latch, because
                    // a latch that keyed on the parsed target would see `ok`
                    // where the armed command saw `alpha` and never match its
                    // own prompt. Anything else in second position is a mistake
                    // worth refusing rather than silently dropping.
                    Rule::word => {
                        let raw = arg.as_str();
                        if ctx.target().is_none() {
                            ctx.set_system("target", Value::Word(raw.to_ascii_lowercase()));
                        } else if raw.eq_ignore_ascii_case(CONFIRM_WORD) {
                            ctx.set_system("confirm", Value::Word(raw.to_ascii_lowercase()));
                        } else {
                            return Err(ParseError::Syntax(format!(
                                "unexpected word {raw:?} after the command word"
                            )));
                        }
                    }
                    other => return Err(ParseError::Syntax(format!("unexpected {other:?}"))),
                }
            }
            Rule::EOI => {}
            other => return Err(ParseError::Syntax(format!("unexpected {other:?}"))),
        }
    }

    Ok(ctx)
}

fn decode(pair: pest::iterators::Pair<Rule>) -> Result<Value, ParseError> {
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| ParseError::Syntax("empty value".into()))?;
    let text = inner.as_str();
    Ok(match inner.as_rule() {
        Rule::int => Value::Int(
            text.parse()
                .map_err(|_| ParseError::Syntax(format!("bad int {text:?}")))?,
        ),
        Rule::float => Value::Float(
            text.parse()
                .map_err(|_| ParseError::Syntax(format!("bad float {text:?}")))?,
        ),
        Rule::bool => Value::Bool(text == "true"),
        // A compound-atomic rule's span includes its delimiters.
        Rule::quoted => Value::Str(text.trim_matches('"').to_string()),
        _ => Value::Word(text.to_string()),
    })
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(s: &str) -> Value {
        Value::Word(s.to_string())
    }

    #[test]
    fn bare_verb_is_a_query() {
        let ctx = parse("garage").unwrap();
        assert_eq!(ctx.verb(), Some("garage"));
        assert_eq!(ctx.target(), None);
    }

    #[test]
    fn second_word_becomes_the_target() {
        let ctx = parse("garage open").unwrap();
        assert_eq!(ctx.verb(), Some("garage"));
        assert_eq!(ctx.target(), Some("open"));
    }

    /// Both are folded to lower case. Not cosmetic: `canonical()` is the latch
    /// key, so `REBOOT AlPhA` and `reboot alpha` have to fold to the same
    /// string or they arm two separate entries.
    #[test]
    fn verb_and_enum_word_are_case_folded() {
        let ctx = parse("GaRaGe Open").unwrap();
        assert_eq!(ctx.verb(), Some("garage"));
        assert_eq!(ctx.target(), Some("open"));
    }

    #[test]
    fn trailing_whitespace_is_tolerated() {
        assert_eq!(parse("garage open  ").unwrap().target(), Some("open"));
    }

    #[test]
    fn the_confirm_token_does_not_disturb_the_target() {
        // The whole point of Phase 0: `ok` must not land on top of `alpha`.
        let ctx = parse("reboot alpha ok").unwrap();
        assert_eq!(ctx.verb(), Some("reboot"));
        assert_eq!(ctx.target(), Some("alpha"));
        assert!(ctx.confirmed());
    }

    #[test]
    fn the_confirm_token_is_case_insensitive() {
        for token in ["ok", "OK", "Ok"] {
            let ctx = parse(&format!("reboot alpha {token}")).unwrap();
            assert_eq!(ctx.target(), Some("alpha"), "{token}");
            assert!(ctx.confirmed(), "{token}");
        }
    }

    #[test]
    fn an_unconfirmed_command_is_not_confirmed() {
        assert!(!parse("reboot alpha").unwrap().confirmed());
        assert!(!parse("garage open").unwrap().confirmed());
    }

    /// A second word that is not the confirm token is a mistake. Dropping it
    /// silently would run a command the user did not type.
    #[test]
    fn a_third_word_is_refused_rather_than_dropped() {
        let err = parse("reboot alpha delta").unwrap_err();
        assert!(
            matches!(&err, ParseError::Syntax(m) if m.contains("delta")),
            "{err:?}"
        );
    }

    /// The gate must not be switchable by the message it gates.
    #[test]
    fn a_message_cannot_set_the_confirm_key() {
        let ctx = parse("reboot alpha confirm=true").unwrap();
        assert!(!ctx.confirmed(), "a slot must not stand in for the token");
        assert!(ctx.is_poisoned("confirm"));
    }

    /// `canonical()` is the latch key, so the confirmation has to fold onto the
    /// entry that armed it, and the injected channel must not leak into it.
    #[test]
    fn canonical_ignores_the_token_and_injected_system_values() {
        let mut armed = parse("REBOOT  alpha").unwrap();
        let mut confirmed = parse("reboot alpha ok").unwrap();
        for ctx in [&mut armed, &mut confirmed] {
            set_system_ctx(ctx);
        }
        assert_eq!(armed.canonical(), "reboot alpha");
        assert_eq!(confirmed.canonical(), armed.canonical());
    }

    fn set_system_ctx(ctx: &mut Context) {
        ctx.set_system("channel", Value::Word("#admin".into()));
        ctx.set_system("channel_idx", Value::Int(3));
    }

    #[test]
    fn slots_are_typed() {
        let ctx = parse("alarm arm code=1664").unwrap();
        assert_eq!(ctx.target(), Some("arm"));
        assert_eq!(ctx.get("code"), Some(&Value::Int(1664)));
    }

    #[test]
    fn every_value_type_round_trips() {
        let ctx = parse(r#"v i=-5 f=1.5 b=true s="two words" w=plain"#).unwrap();
        assert_eq!(ctx.get("i"), Some(&Value::Int(-5)));
        assert_eq!(ctx.get("f"), Some(&Value::Float(1.5)));
        assert_eq!(ctx.get("b"), Some(&Value::Bool(true)));
        assert_eq!(ctx.get("s"), Some(&Value::Str("two words".into())));
        assert_eq!(ctx.get("w"), Some(&word("plain")));
    }

    #[test]
    fn control_characters_are_rejected() {
        for c in ['\n', '\r', '\t'] {
            let err = parse(&format!("garage open{c}")).unwrap_err();
            assert_eq!(err, ParseError::IllegalChar(c));
        }
    }

    #[test]
    fn over_length_input_is_rejected() {
        let long = "a".repeat(MAX_INPUT_BYTES + 1);
        assert_eq!(
            parse(&long).unwrap_err(),
            ParseError::TooLong {
                len: MAX_INPUT_BYTES + 1,
                max: MAX_INPUT_BYTES
            }
        );
    }

    #[test]
    fn path_traversal_cannot_ride_in_a_word() {
        // `/` is excluded so no value can be a path; `../` never parses.
        assert!(matches!(
            parse("garage ../etc/passwd").unwrap_err(),
            ParseError::Syntax(_)
        ));
    }

    #[test]
    fn a_second_equals_cannot_hide_inside_a_word() {
        assert!(matches!(
            parse("v a=1 b=2=3").unwrap_err(),
            ParseError::Syntax(_)
        ));
    }

    #[test]
    fn reserved_keys_cannot_be_spoofed() {
        let mut ctx = parse("garage channel=#admin").unwrap();
        ctx.set_system("channel", word("#homeassistant"));
        assert_eq!(ctx.get("channel"), Some(&word("#homeassistant")));
        assert!(ctx.is_poisoned("channel"));
        assert_eq!(ctx.poisoned_keys().collect::<Vec<_>>(), vec!["channel"]);
    }

    #[test]
    fn a_message_cannot_overwrite_the_verb() {
        let ctx = parse("garage verb=alarm").unwrap();
        assert_eq!(ctx.verb(), Some("garage"));
        assert!(ctx.is_poisoned("verb"));
    }

    #[test]
    fn a_slot_cannot_forge_the_target() {
        let ctx = parse("garage target=close").unwrap();
        assert_eq!(ctx.target(), None);
        assert!(ctx.is_poisoned("target"));
    }

    #[test]
    fn env_keys_can_be_reserved_too() {
        let mut ctx = Context::new_with(&Reserved::new(["PVE_TOKEN"]));
        ctx.set_system("PVE_TOKEN", word("s3cret"));
        ctx.set_message("pve_token", word("stolen"));
        assert_eq!(ctx.get("pve_token"), Some(&word("s3cret")));
        assert!(ctx.is_poisoned("pve_token"));
    }

    #[test]
    fn a_digit_run_inside_a_word_is_not_an_integer() {
        let ctx = parse("v code=5abc").unwrap();
        assert_eq!(ctx.get("code"), Some(&word("5abc")));
    }

    #[test]
    fn a_keyword_prefix_is_a_word_not_a_partial_keyword() {
        let ctx = parse("v b=truely").unwrap();
        assert_eq!(ctx.get("b"), Some(&word("truely")));
    }

    #[test]
    fn duplicate_slots_are_rejected() {
        assert_eq!(
            parse("v a=1 a=2").unwrap_err(),
            ParseError::DuplicateKey("a".into())
        );
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(parse("").is_err());
    }

    #[test]
    fn canonical_sorts_args_so_equivalent_commands_share_a_latch() {
        let a = parse("reboot alpha delay=5").unwrap();
        let b = parse("reboot delay=5 alpha").unwrap();
        assert_eq!(a.canonical(), "reboot alpha delay=5");
        assert_eq!(a.canonical(), b.canonical());
    }
}
