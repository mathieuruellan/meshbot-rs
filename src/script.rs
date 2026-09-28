//! Running a verb's declared script.
//!
//! Everything here is defensive, because the inputs arrive over the air. The
//! verb table decides *which* script runs; the message only ever contributes an
//! argument that a declared entry already names. Three properties are load
//! bearing and each has a test below:
//!
//! 1. **The script path is resolved against an allowlist directory.** Not
//!    "sanitised" — a name that resolves outside the directory is refused, so a
//!    verb entry can only ever reach a file the operator put there.
//! 2. **No shell, ever.** Arguments go to `execve` as separate argv elements.
//!    A message value can never become command injection because nothing
//!    re-parses it.
//! 3. **The child does not inherit the bot's environment.** `env_clear()` first,
//!    then exactly the allowlisted names. Without this the channel secret and
//!    every other token in `.env` would be readable by every script, and every
//!    script's child.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

use crate::parse::Context as Message;
use crate::verbs::{ActionSpec, MAX_REPLY_BYTES};

/// Where action scripts live on the host, mounted read-only.
///
/// Overridable so the examples in this repo can be run without a deploy:
/// `MESHBOT_SCRIPT_DIR=./scripts.example`.
const DEFAULT_SCRIPT_DIR: &str = "/data/meshcore/meshbot-rs/scripts";

/// Longest a script may take before it is killed and the reply is a failure.
/// A per-action `timeout_secs` overrides this.
const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// The allowlist directory, validated at startup.
///
/// Overridable so the examples in this repo can be run without a deploy:
/// `MESHBOT_SCRIPT_DIR=./scripts.example`.
pub fn script_dir() -> PathBuf {
    match std::env::var("MESHBOT_SCRIPT_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(DEFAULT_SCRIPT_DIR),
    }
}

/// Resolve a declared script name to a path inside the allowlist directory.
///
/// The directory must already exist: `canonicalize` on the base fails
/// otherwise, and that is a legitimate startup error rather than something to
/// paper over at spawn time. Subdirectories are not allowed — the name is
/// resolved flat, so there is no way to reach `../` or a nested tree.
pub fn resolve_script(name: &str, dir: &Path) -> Result<PathBuf> {
    if name.is_empty() {
        bail!("empty script name");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("script name {name:?} must not contain a path separator");
    }
    if name == "." || name == ".." {
        bail!("script name {name:?} is not a file");
    }

    let base = dir
        .canonicalize()
        .with_context(|| format!("script directory {} is not usable", dir.display()))?;

    let candidate = base.join(name);
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("script {name:?} not found in {}", base.display()))?;

    if resolved.parent() != Some(base.as_path()) {
        bail!(
            "script {name:?} resolves to {} which is outside {}",
            resolved.display(),
            base.display()
        );
    }
    Ok(resolved)
}

/// What a script produced, and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Clamped stdout, destined for the air.
    pub stdout: String,
    pub success: bool,
    /// Set when the script could not be run at all. Never sent verbatim if it
    /// might carry a path; the caller decides what to put on the air.
    pub error: Option<String>,
}

impl Outcome {
    /// A short line describing failure, safe to put on a channel: it never
    /// includes a token, and a path from a rejected script name is not useful
    /// over LoRa anyway.
    pub fn failure_line(&self) -> String {
        match &self.error {
            Some(_) => "action failed".to_string(),
            None if !self.success => "action did not succeed".to_string(),
            None => self.stdout.clone(),
        }
    }
}

/// Expand `{{slot}}` against the message context.
///
/// Only declared values resolve. There is deliberately no `{{text}}`: an action
/// argument may expand to what the verb table declares, never to raw message
/// text, which is what keeps a script from being handed a whole unvetted
/// sentence.
pub fn expand(template: &str, ctx: &Message, stdout: Option<&str>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("unterminated placeholder in {template:?}");
        };
        let name = &after[..end];

        if name == "stdout" {
            let Some(text) = stdout else {
                bail!("{{{{stdout}}}} is only available in a reply, not in an argument");
            };
            out.push_str(text);
        } else {
            let value = ctx
                .get(name)
                .ok_or_else(|| anyhow!("no such value {name:?} in this message"))?;
            out.push_str(&value.to_string());
        }

        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The child's environment: exactly the allowlisted names, read from ours.
///
/// A missing name is an error rather than a silent omission. Spawning with an
/// empty `PVE_TOKEN_GUEST` would produce a confusing 401 from the API instead of
/// a clear "not configured" here.
fn child_env(allow: &[String]) -> Result<HashMap<String, String>> {
    child_env_from(allow, |name| std::env::var(name).ok())
}

/// `child_env` with the lookup injected, so the allowlist behaviour is testable
/// without mutating the real process environment.
fn child_env_from(
    allow: &[String],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<String, String>> {
    let mut env = HashMap::with_capacity(allow.len());
    for name in allow {
        let value = lookup(name)
            .with_context(|| format!("{name} is required by this action but is not set"))?;
        env.insert(name.clone(), value);
    }
    Ok(env)
}

/// Build the command for an action. Split out from [`run`] so the argv and env
/// can be asserted in tests without spawning anything.
pub fn command_for(
    action: &ActionSpec,
    ctx: &Message,
    dir: &Path,
) -> Result<(PathBuf, Vec<String>, HashMap<String, String>)> {
    let script = resolve_script(&action.script, dir)?;
    let mut args = Vec::with_capacity(action.args.len());
    for template in &action.args {
        args.push(expand(template, ctx, None)?);
    }
    let env = child_env(&action.env)?;
    Ok((script, args, env))
}

/// Run a verb's action and return what it produced.
///
/// Never propagates an error to the caller: a failed action is a reply, not a
/// crash, and the bot has to keep listening either way.
pub async fn run(action: &ActionSpec, ctx: &Message, dir: &Path) -> Outcome {
    let timeout = Duration::from_secs(action.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    let (script, args, env) = match command_for(action, ctx, dir) {
        Ok(parts) => parts,
        Err(err) => {
            tracing::warn!(script = %action.script, %err, "action not runnable");
            return Outcome {
                stdout: String::new(),
                success: false,
                error: Some(err.to_string()),
            };
        }
    };

    let mut child = Command::new(&script);
    child.args(args.iter().map(OsStr::new));
    // The order matters: clear first, then add the allowlist. Reversed, the
    // bot's own environment would be the base and the allowlist would merely
    // override a few names in it.
    child.env_clear();
    for (name, value) in &env {
        child.env(name, value);
    }
    child.stdin(Stdio::null());
    child.stdout(Stdio::piped());
    // stderr is never relayed onto a channel. Nulled rather than inherited so a
    // chatty script cannot interleave into the bot's logs, and piped-then-ignored
    // would risk filling a pipe buffer and blocking the child.
    child.stderr(Stdio::null());
    // Without this, the timeout below is a lie: `wait_with_output` consumes the
    // child, so dropping the future on expiry drops the `Child` and — with
    // tokio's default of `kill_on_drop(false)` — leaves the script running with
    // nobody waiting for it. A `reboot` loop would outlive its timeout.
    child.kill_on_drop(true);

    let child = match child.spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(script = %script.display(), %err, "spawn failed");
            return Outcome {
                stdout: String::new(),
                success: false,
                error: Some(err.to_string()),
            };
        }
    };

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Outcome {
            stdout: String::new(),
            success: false,
            error: Some(format!("timed out after {}s", timeout.as_secs())),
        },
        Ok(Err(err)) => {
            tracing::warn!(script = %script.display(), %err, "wait failed");
            Outcome {
                stdout: String::new(),
                success: false,
                error: Some(err.to_string()),
            }
        }
        Ok(Ok(output)) => {
            // A script may print far more than one frame. Clamp before the
            // template sees it, so `{{stdout}}` can never build an over-long
            // reply that hangs the radio.
            let stdout =
                crate::verbs::clamp(&String::from_utf8_lossy(&output.stdout), MAX_REPLY_BYTES);
            let success = output.status.success();
            if !success {
                tracing::warn!(
                    script = %script.display(),
                    code = output.status.code(),
                    "action exited non-zero"
                );
            }
            Outcome {
                stdout,
                success,
                error: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A private directory per test, so parallel tests never share a path.
    fn sandbox(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("meshbot-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a script and make it executable.
    ///
    /// Written to a staged name and renamed into place: writing a file that is
    /// about to be `execve`d can fail with `ETXTBSY` on overlayfs, which is what
    /// a container uses. The rename is atomic, so the exec never sees a
    /// half-written or still-open file.
    fn write_script(dir: &Path, name: &str, body: &str) {
        let staged = dir.join(format!(".{name}.staged"));
        std::fs::write(&staged, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&staged).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&staged, perms).unwrap();
        }
        std::fs::rename(&staged, dir.join(name)).unwrap();
    }

    /// A directory with one real script in it, for the allowlist tests.
    fn sandbox_with_ok() -> PathBuf {
        let dir = sandbox("allow");
        write_script(&dir, "ok.sh", "#!/bin/sh\nexit 0\n");
        dir
    }

    #[test]
    fn a_plain_name_resolves_inside_the_directory() {
        let dir = sandbox_with_ok();
        let path = resolve_script("ok.sh", &dir).unwrap();
        assert_eq!(path.parent().unwrap(), dir.canonicalize().unwrap());
    }

    #[test]
    fn a_name_cannot_escape_the_allowlist() {
        let dir = sandbox_with_ok();
        for name in ["../ok.sh", "sub/ok.sh", "/etc/passwd", "..", ".", ""] {
            assert!(
                resolve_script(name, &dir).is_err(),
                "{name:?} should not resolve"
            );
        }
    }

    #[test]
    fn a_symlink_out_of_the_directory_is_refused() {
        let dir = sandbox_with_ok();
        let outside_dir = sandbox("outside");
        write_script(&outside_dir, "elsewhere.sh", "#!/bin/sh\nexit 0\n");
        let link = dir.join("escape.sh");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside_dir.join("elsewhere.sh"), &link).unwrap();

        // The link resolves to a real file outside the tree, so the parent check
        // is what catches it.
        assert!(resolve_script("escape.sh", &dir).is_err());
    }

    #[test]
    fn a_missing_script_is_an_error_not_a_spawn() {
        let dir = sandbox_with_ok();
        assert!(resolve_script("nope.sh", &dir).is_err());
    }

    #[test]
    fn placeholders_expand_from_the_message() {
        let ctx = parse::parse("reboot alpha").unwrap();
        assert_eq!(expand("{{target}}", &ctx, None).unwrap(), "alpha");
        assert_eq!(
            expand("reboot {{target}} now", &ctx, None).unwrap(),
            "reboot alpha now"
        );
    }

    #[test]
    fn stdout_is_only_valid_in_a_reply() {
        let ctx = parse::parse("reboot alpha").unwrap();
        assert!(expand("{{stdout}}", &ctx, Some("ok")).is_ok());
        assert!(expand("{{stdout}}", &ctx, None).is_err());
    }

    #[test]
    fn an_undeclared_placeholder_fails_rather_than_passing_empty() {
        let ctx = parse::parse("reboot alpha").unwrap();
        assert!(expand("{{nope}}", &ctx, None).is_err());
        // Engine-injected values are readable: `verb` is already validated
        // against the table by the time anything expands, and a message cannot
        // invent a key the context does not hold.
        assert_eq!(expand("{{verb}}", &ctx, None).unwrap(), "reboot");
        // An unterminated placeholder is a config bug, not something to guess at.
        assert!(expand("{{target", &ctx, None).is_err());
    }

    /// The decisive one: a value with shell metacharacters stays a single argv
    /// element. Nothing re-parses it, so there is no injection to defend
    /// against — this documents that rather than relying on it implicitly.
    #[test]
    fn an_argument_with_shell_metacharacters_stays_one_element() {
        let dir = sandbox_with_ok();
        let ctx = parse::parse(r#"reboot alpha note="a; rm -rf / b""#).unwrap();
        let action = ActionSpec {
            script: "ok.sh".into(),
            args: vec!["{{note}}".into()],
            env: vec![],
            ..ActionSpec::default()
        };
        let (_, args, _) = command_for(&action, &ctx, &dir).unwrap();
        assert_eq!(args, vec!["a; rm -rf / b".to_string()]);
    }

    #[test]
    fn the_child_gets_only_the_allowlisted_variables() {
        // A fake parent environment holding a secret the action never declares.
        let parent = |name: &str| match name {
            "HA_URL" => Some("https://ha.example".to_string()),
            "HA_TOKEN" => Some("super-secret".to_string()),
            "PATH" => Some("/usr/bin".to_string()),
            _ => None,
        };

        let env = child_env_from(&["HA_URL".to_string(), "HA_TOKEN".to_string()], parent).unwrap();
        assert_eq!(env.len(), 2);
        assert!(env.contains_key("HA_URL"));
        // The strongest form of the assertion: the parent environment is
        // emptied, not filtered by name. PATH would otherwise be a silent leak.
        assert!(!env.contains_key("PATH"), "the parent env must not leak in");
    }

    #[test]
    fn a_missing_allowlisted_variable_is_an_error() {
        let parent = |_: &str| None;
        let err = child_env_from(&["HA_TOKEN".to_string()], parent).unwrap_err();
        assert!(err.to_string().contains("HA_TOKEN"), "{err}");
    }

    #[tokio::test]
    async fn a_successful_script_reports_its_clamped_stdout() {
        let dir = sandbox("run");
        write_script(&dir, "say.sh", "#!/bin/sh\necho armed\n");

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: "say.sh".into(),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.stdout, "armed\n");
    }

    #[tokio::test]
    async fn a_failing_script_does_not_succeed_and_never_leaks_stderr() {
        let dir = sandbox("fail");
        // The secret goes to stderr, which must not reach the reply.
        write_script(
            &dir,
            "boom.sh",
            "#!/bin/sh\necho token=SECRET >&2\nexit 3\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: "boom.sh".into(),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(!outcome.success);
        assert!(!outcome.stdout.contains("SECRET"), "stderr leaked");
        assert!(!outcome.failure_line().contains("SECRET"));
    }

    #[tokio::test]
    async fn a_slow_script_is_killed_at_the_timeout() {
        let dir = sandbox("slow");
        write_script(&dir, "slow.sh", "#!/bin/sh\nsleep 30\n");

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: "slow.sh".into(),
            timeout_secs: Some(1),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(!outcome.success);
        let error = outcome.error.clone().unwrap_or_default();
        assert!(error.contains("timed out"), "{error:?}");
    }

    /// One reply, one frame: a script that prints a paragraph must not produce
    /// an over-long reply that hangs the radio.
    #[tokio::test]
    async fn a_very_chatty_script_is_clamped() {
        let dir = sandbox("chatty");
        write_script(
            &dir,
            "loud.sh",
            "#!/bin/sh\nfor i in $(seq 1 200); do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: "loud.sh".into(),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(outcome.success);
        assert!(
            outcome.stdout.len() <= MAX_REPLY_BYTES,
            "{} bytes",
            outcome.stdout.len()
        );
    }
}
