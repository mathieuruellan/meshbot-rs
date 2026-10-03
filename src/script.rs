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
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::parse::Context as Message;
use crate::verbs::ActionSpec;

/// Longest a script may take before it is killed and the reply is a failure.
/// A per-action `timeout_secs` overrides this.
const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// Number of spawns left to fail on purpose, for the tests.
///
/// The branch this reaches — the one that answers `action failed` because nothing
/// ran at all — is otherwise reachable only by breaking the machine: a fork that
/// fails, or an `ETXTBSY` on a busy runner. CI hit exactly that and the test that
/// noticed it could only report the two strings it had, not why they differed.
#[cfg(test)]
static SPAWN_FAILURES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
    /// stdout, destined for the air — **one message per line**, each clamped by
    /// [`crate::verbs::split_reply`] rather than here. Clamping the whole blob
    /// would truncate a script that reports several things mid-list, so the
    /// budget is applied per line at the point it is turned into messages.
    pub stdout: String,
    /// Clamped stderr, destined for the air **only when the action failed** —
    /// see [`Outcome::failure_line`]. A script therefore has to keep both
    /// streams safe to broadcast: a token, a host id or a URL on stderr is a
    /// leak the moment the action exits non-zero.
    pub stderr: String,
    pub success: bool,
    /// Set when the script could not be run at all. Never sent verbatim if it
    /// might carry a path; the caller decides what to put on the air.
    pub error: Option<String>,
}

impl Outcome {
    /// A short line describing failure, safe to put on a channel: it never
    /// includes a token, and a path from a rejected script name is not useful
    /// over LoRa anyway.
    ///
    /// The script's own last stderr line is preferred over any generic string,
    /// because "action did not succeed" tells an operator nothing about a
    /// command they asked for by name. `stderr` is the script's words, so this
    /// is only as safe as the script is — hence the contract that both streams
    /// stay free of credentials, ids and URLs.
    pub fn failure_line(&self) -> String {
        if self.success {
            return self.stdout.clone();
        }
        last_line(&self.stderr).unwrap_or_else(|| match &self.error {
            Some(_) => "action failed".to_string(),
            None => "action did not succeed".to_string(),
        })
    }
}

/// Longest a script's stderr may occupy in a reply, so a chatty failure cannot
/// eat the whole frame the failure needs.
const MAX_STDERR_BYTES: usize = 100;

/// The last non-empty line of a script's stderr, trimmed and clamped.
///
/// A script is free to write several lines of diagnostics; only the last is
/// kept, because that is the one that names the failure. Earlier lines are
/// progress or noise, and the frame is scarce.
fn last_line(stderr: &str) -> Option<String> {
    let line = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(crate::verbs::clamp(line, MAX_STDERR_BYTES))
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
    let Some(name) = action.script.as_deref() else {
        bail!("reply-only action has no script to run");
    };
    let script = resolve_script(name, dir)?;
    let mut args = Vec::with_capacity(action.args.len());
    for template in &action.args {
        args.push(expand(template, ctx, None)?);
    }
    let env = child_env(&action.env)?;
    Ok((script, args, env))
}

/// Fork the child, honouring the test-only injection in [`SPAWN_FAILURES`].
///
/// Split out so the injection sits next to the one `spawn` it replaces. In a
/// normal build this is `command.spawn()` and nothing else: the counter is
/// `#[cfg(test)]`, so no shipped binary pays for it.
#[cfg(not(test))]
fn spawn_child(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    command.spawn()
}

/// `SPAWN_FAILURES` is a countdown, decremented by the test that armed it. It is
/// armed only while the process-spawning tests hold `SPAWN_SLOT`, so "the next
/// spawn" is unambiguous.
#[cfg(test)]
fn spawn_child(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    use std::sync::atomic::Ordering;
    let armed = SPAWN_FAILURES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
            left.checked_sub(1)
        })
        .is_ok();
    if armed {
        return Err(std::io::Error::other("spawn failed (injected by test)"));
    }
    command.spawn()
}

/// Run a verb's action and return what it produced.
///
/// Never propagates an error to the caller: a failed action is a reply, not a
/// crash, and the bot has to keep listening either way.
pub async fn run(action: &ActionSpec, ctx: &Message, dir: &Path) -> Outcome {
    // A reply-only action runs nothing and succeeds: the caller renders its
    // template from the message context, and `{{stdout}}` is empty.
    if action.script.is_none() {
        return Outcome {
            stdout: String::new(),
            stderr: String::new(),
            success: true,
            error: None,
        };
    }

    let timeout = Duration::from_secs(action.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    let (script, args, env) = match command_for(action, ctx, dir) {
        Ok(parts) => parts,
        Err(err) => {
            tracing::warn!(script = ?action.script, %err, "action not runnable");
            return Outcome {
                stdout: String::new(),
                stderr: String::new(),
                success: false,
                error: Some(err.to_string()),
            };
        }
    };

    let mut command = Command::new(&script);
    command.args(args.iter().map(OsStr::new));
    // The order matters: clear first, then add the allowlist. Reversed, the
    // bot's own environment would be the base and the allowlist would merely
    // override a few names in it.
    command.env_clear();
    for (name, value) in &env {
        command.env(name, value);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    // Piped rather than nulled: a failing action's last stderr line becomes the
    // reply, which is the only diagnostic an operator asking for a reboot by
    // name ever sees. Both pipes are drained by tasks below, so a chatty script
    // cannot fill a buffer and block, and its output cannot interleave into the
    // bot's own logs. On success stderr is logged and not sent.
    command.stderr(Stdio::piped());
    // The child is killed explicitly on timeout. `kill_on_drop` stays on as a
    // backstop for the paths that return early, so no future can leave a script
    // running past its deadline.
    command.kill_on_drop(true);

    let mut child = match spawn_child(&mut command) {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(script = %script.display(), %err, "spawn failed");
            return Outcome {
                stdout: String::new(),
                stderr: String::new(),
                success: false,
                error: Some(err.to_string()),
            };
        }
    };

    // Take the pipes before waiting. `wait_with_output` would consume the child
    // and return everything at once, but it also means a timeout drops the
    // future and the output with it — and a timeout is exactly when the
    // diagnosis is worth having. Draining in tasks instead keeps whatever the
    // script managed to write before it was killed.
    let out_task = child.stdout.take().map(|pipe| tokio::spawn(drain(pipe)));
    let err_task = child.stderr.take().map(|pipe| tokio::spawn(drain(pipe)));

    // `status` is the exit status when the script finished; `error` is why it
    // did not, when it did not. The two are separate because a timeout is a
    // reportable fact of its own rather than a missing result.
    let (status, error) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (Some(status), None),
        Ok(Err(err)) => {
            tracing::warn!(script = %script.display(), %err, "wait failed");
            (None, Some(err.to_string()))
        }
        Err(_) => {
            // `child` is still owned here, so the kill is explicit and the exit
            // status is reaped rather than left as a zombie.
            let _ = child.start_kill();
            let _ = child.wait().await;
            (
                None,
                Some(format!("timed out after {}s", timeout.as_secs())),
            )
        }
    };

    let stdout = join(out_task).await;
    let stderr = join(err_task).await;

    let Some(status) = status else {
        // Killed or interrupted. Whatever it wrote before that is still the
        // most useful thing on the air, so it is kept even though the run
        // itself produced no result.
        return Outcome {
            stdout: String::new(),
            stderr: crate::verbs::clamp(&stderr, MAX_STDERR_BYTES),
            success: false,
            error,
        };
    };

    // stderr is still clamped as one blob: only its last line is ever relayed,
    // and that is a single message however much the script wrote.
    let stderr = crate::verbs::clamp(&stderr, MAX_STDERR_BYTES);
    let success = status.success();

    if success {
        // Logged, never relayed: a dry run explains itself on stderr and that
        // explanation must stay off the channel.
        if !stderr.is_empty() {
            tracing::debug!(script = %script.display(), %stderr, "stderr on success");
        }
    } else {
        tracing::warn!(
            script = %script.display(),
            code = status.code(),
            %stderr,
            "action exited non-zero"
        );
    }

    Outcome {
        stdout,
        stderr,
        success,
        error: None,
    }
}

/// Read a child's pipe to EOF, keeping only the first 4 KB.
///
/// The rest is still drained — a script that prints megabytes must not block
/// forever on a full pipe — but it is discarded rather than buffered, so a
/// runaway script cannot grow this process's memory.
async fn drain<R: AsyncRead + Unpin>(mut pipe: R) -> String {
    const MAX_CAPTURE_BYTES: usize = 4096;

    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Wait for a drain task, falling back to empty output if it panicked.
async fn join(task: Option<JoinHandle<String>>) -> String {
    match task {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use crate::verbs::MAX_REPLY_BYTES;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// Held by every test that forks a child, for the length of that test.
    ///
    /// Serialised on purpose. A `#[tokio::test]` runs on its own current-thread
    /// runtime, so N of them forking at once means N runtimes and N pairs of
    /// pipes in one process — and an occasional fork that fails, or a pipe close
    /// that arrives late, then shows up as a test failure with nothing in the
    /// message. That is not hypothetical: CI saw this module assert the wrong
    /// string because a spawn failed for a reason the test could not see. None of
    /// these tests measure anything about each other, and the whole module costs
    /// milliseconds once the sleeping ones stop sleeping.
    ///
    /// Tokio's mutex rather than `std`'s, because the guard is held across the
    /// test's awaits by definition. It does not poison, which is what is wanted:
    /// these tests share no state, so a panic in one says nothing about the next
    /// one's sandbox.
    static SPAWN_SLOT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn spawn_slot() -> tokio::sync::MutexGuard<'static, ()> {
        SPAWN_SLOT.lock().await
    }

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
            script: Some("ok.sh".into()),
            args: vec!["{{note}}".into()],
            env: vec![],
            ..ActionSpec::default()
        };
        let (_, args, _) = command_for(&action, &ctx, &dir).unwrap();
        assert_eq!(args, vec!["a; rm -rf / b".to_string()]);
    }

    /// End to end, through the real table: a typed word becomes exactly the
    /// argv the script expects, with the id and kind coming from the table and
    /// never from the message.
    ///
    /// This is the whole reason the map moved out of the script, so it is worth
    /// pinning at this level rather than in the table's own test: what matters is
    /// the command line a person on a channel can cause.
    #[test]
    fn each_declared_word_produces_the_command_its_entry_declares() {
        let dir = sandbox("reboot-argv");
        write_script(&dir, "pve-reboot.sh", "#!/bin/sh\nexit 0\n");
        let table = &crate::config::example().table;
        let reboot = table.get("reboot").expect("reboot is declared");

        let expected = [
            ("alpha", vec!["alpha", "100", "qemu"]),
            ("beta", vec!["beta", "101", "qemu"]),
            ("gamma", vec!["gamma", "102", "lxc"]),
            ("delta", vec!["delta", "103", "qemu"]),
            ("komodo", vec!["komodo", "104", "lxc"]),
            // The node: no vmid segment in the API path, and no id on the air.
            ("pve", vec!["pve", "-", "host"]),
        ];

        for (word, argv) in expected {
            let ctx = parse::parse(&format!("reboot {word}")).unwrap();
            let action = &reboot
                .args
                .iter()
                .find(|arg| arg.words.iter().any(|w| w == word))
                .unwrap_or_else(|| panic!("{word} is not declared"))
                .action;
            // The env allowlist is emptied for this assertion: `command_for`
            // reads it from the test process's own environment, and no test
            // should mutate that. What is under test here is the argv.
            let action = ActionSpec {
                env: vec![],
                ..action.clone()
            };
            let (script, args, _) = command_for(&action, &ctx, &dir).unwrap();
            assert_eq!(args, argv, "{word}");
            assert_eq!(script.file_name().unwrap(), "pve-reboot.sh");
        }
    }

    /// A word the table does not declare resolves to no action at all, so there
    /// is nothing to build a command from. This is the enum doing its job: the
    /// message can pick a machine, never invent one.
    #[test]
    fn an_undeclared_word_fires_nothing() {
        let table = &crate::config::example().table;
        let ctx = parse::parse("reboot myPersonalServer").unwrap();
        assert!(!table.resolve(&ctx).is_actionable());
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
    async fn a_successful_script_reports_its_stdout() {
        let _slot = spawn_slot().await;
        let dir = sandbox("run");
        write_script(&dir, "say.sh", "#!/bin/sh\necho armed\n");

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("say.sh".into()),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.stdout, "armed\n");
    }

    /// Every line survives, because `split_reply` is what decides how many
    /// messages there are and it needs to see them all. Clamping here would
    /// truncate a five-machine list at 150 bytes and the caller would never know
    /// two entries were missing.
    #[tokio::test]
    async fn a_multiline_stdout_is_preserved_intact() {
        let _slot = spawn_slot().await;
        let dir = sandbox("multiline");
        write_script(
            &dir,
            "say.sh",
            "#!/bin/sh\necho '1/3 server a not ok'\necho '2/3 stack b unhealthy'\necho '3/3 stack c down'\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("say.sh".into()),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(
            outcome.stdout.lines().collect::<Vec<_>>(),
            [
                "1/3 server a not ok",
                "2/3 stack b unhealthy",
                "3/3 stack c down"
            ]
        );
    }

    /// A failing action answers with the script's own last line of stderr.
    ///
    /// This replaced the old rule, where stderr was discarded and every failure
    /// read `action did not succeed`. That told an operator nothing about a command
    /// they asked for by name. The rule is still fail-closed in the way that
    /// matters: a failed run never renders its stdout, so a partial result is never
    /// broadcast as if it were the outcome.
    #[tokio::test]
    async fn a_failing_script_replies_with_its_last_stderr_line() {
        let _slot = spawn_slot().await;
        let dir = sandbox("fail");
        write_script(
            &dir,
            "boom.sh",
            "#!/bin/sh\necho 'looking up the task' >&2\necho 'guest 112 is locked' >&2\nexit 3\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("boom.sh".into()),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(
            outcome.error.is_none(),
            "the script did not run: {outcome:?}"
        );
        assert!(!outcome.success);
        assert!(
            !outcome.failure_line().contains("looking up"),
            "only the last line is sent"
        );
        assert_eq!(outcome.failure_line(), "guest 112 is locked");
    }

    #[tokio::test]
    async fn a_failure_with_nothing_on_stderr_stays_generic() {
        let _slot = spawn_slot().await;
        let dir = sandbox("silent-fail");
        write_script(&dir, "quiet-fail.sh", "#!/bin/sh\nexit 3\n");

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("quiet-fail.sh".into()),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        // Split out from the string comparison on purpose: this line and the one
        // below differ by whether the script ran, and an assert that folds both
        // into one string reports the difference without ever saying what caused
        // it. CI learned that the expensive way.
        assert!(
            outcome.error.is_none(),
            "the script did not run: {outcome:?}"
        );
        assert_eq!(outcome.failure_line(), "action did not succeed");
    }

    /// A script that never started says so, and says nothing else.
    ///
    /// `action failed` is reserved for exactly this case — nothing ran — as
    /// against `action did not succeed`, which is a script that ran and failed.
    /// Both are reachable in production (a missing file, a variable the verb
    /// entry declares but `.env` does not have, a fork that fails), and neither
    /// leaves anything on stdout or stderr to relay.
    ///
    /// The branch used to be reachable only by breaking the machine, so the line
    /// above had no test and CI could hit it unannounced.
    #[tokio::test]
    async fn a_script_that_never_started_says_nothing_ran() {
        let _slot = spawn_slot().await;
        let dir = sandbox("no-spawn");
        write_script(&dir, "never.sh", "#!/bin/sh\necho 'this never runs'\n");

        SPAWN_FAILURES.store(1, Ordering::Relaxed);
        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("never.sh".into()),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(!outcome.success);
        assert_eq!(outcome.stdout, "");
        assert_eq!(outcome.stderr, "");
        assert!(outcome.error.is_some(), "{outcome:?}");
        assert_eq!(outcome.failure_line(), "action failed");
        // Nothing left armed for the next test in this process to inherit.
        assert_eq!(SPAWN_FAILURES.load(Ordering::Relaxed), 0);
    }

    /// The other way to reach the same line: the verb entry names a variable that
    /// is not set, so the action is refused before anything is forked.
    ///
    /// `env_clear()` means the child would not have had the variable either, which
    /// is why a missing one is a config error rather than a script that has to
    /// cope. The name cannot be set in the test environment either, asserted
    /// below so that stays true.
    #[tokio::test]
    async fn a_missing_allowlisted_variable_stops_the_action_before_it_spawns() {
        let _slot = spawn_slot().await;
        let dir = sandbox("no-env");
        write_script(&dir, "ok.sh", "#!/bin/sh\necho armed\n");
        assert!(std::env::var("MESHBOT_TEST_ABSENT").is_err());

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("ok.sh".into()),
            env: vec!["MESHBOT_TEST_ABSENT".into()],
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(!outcome.success);
        assert_eq!(outcome.stdout, "");
        assert!(outcome.error.is_some(), "{outcome:?}");
        assert_eq!(outcome.failure_line(), "action failed");
    }

    /// The reason for draining the pipes by hand rather than with
    /// `wait_with_output`: a timeout used to discard the output along with the
    /// future, which is exactly when a diagnosis is worth having.
    ///
    /// The `sleep` is redirected to `/dev/null` so this measures the deadline and
    /// nothing else. `run` waits for the pipes to reach EOF, and a grandchild that
    /// inherits them keeps them open after the shell is killed — which is a real
    /// gap in `run`, recorded in `AGENTS.md`, and a 30-second test rather than a
    /// one-second one.
    #[tokio::test]
    async fn a_script_killed_at_the_timeout_still_explains_itself() {
        let _slot = spawn_slot().await;
        let dir = sandbox("slow-fail");
        write_script(
            &dir,
            "slow-fail.sh",
            "#!/bin/sh\necho 'still polling the task' >&2\nsleep 30 >/dev/null 2>&1\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("slow-fail.sh".into()),
            timeout_secs: Some(1),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(!outcome.success);
        assert_eq!(outcome.failure_line(), "still polling the task");
        let error = outcome.error.unwrap_or_default();
        assert!(error.contains("timed out"), "{error:?}");
    }

    /// A script that succeeds may be as chatty as it likes on stderr: the detail
    /// stays in the log. `pve-reboot.sh`'s dry run depends on this — it prints the
    /// request it would have sent, which must never reach a channel.
    #[tokio::test]
    async fn a_successful_scripts_stderr_stays_off_the_air() {
        let _slot = spawn_slot().await;
        let dir = sandbox("chatty-ok");
        write_script(
            &dir,
            "chatty-ok.sh",
            "#!/bin/sh\necho 'would POST https://pve.example/api2/json/nodes/pve/qemu/112' >&2\necho 'dry-run: nothing sent'\nexit 0\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("chatty-ok.sh".into()),
            reply: "{{stdout}}".into(),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(outcome.success, "{outcome:?}");
        assert_eq!(outcome.stdout, "dry-run: nothing sent\n");
        assert!(outcome.stderr.contains("would POST"));
        // What the caller puts on the air is the success path, so it is stdout.
        assert!(!outcome.stdout.contains("pve.example"));
    }

    /// A chatty failure cannot eat the frame the failure needs.
    #[tokio::test]
    async fn a_relayed_stderr_line_is_clamped() {
        let _slot = spawn_slot().await;
        let dir = sandbox("loud-fail");
        write_script(
            &dir,
            "loud-fail.sh",
            &format!("#!/bin/sh\necho 'b {} c' >&2\nexit 3\n", "x".repeat(400)),
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("loud-fail.sh".into()),
            ..ActionSpec::default()
        };
        let reply = {
            let outcome = run(&action, &ctx, &dir).await;
            assert!(
                outcome.error.is_none(),
                "the script did not run: {outcome:?}"
            );
            outcome.failure_line()
        };
        assert!(
            reply.len() <= MAX_STDERR_BYTES,
            "{} bytes: {reply}",
            reply.len()
        );
        assert!(reply.starts_with("b xxx"));
    }

    /// A script that prints more than the capture limit is drained rather than
    /// allowed to fill its pipe and deadlock, and what is kept is the beginning.
    #[tokio::test]
    async fn a_script_that_floods_stderr_does_not_deadlock() {
        let _slot = spawn_slot().await;
        let dir = sandbox("flood");
        write_script(
            &dir,
            "flood.sh",
            "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' 'x'\nexit 3\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("flood.sh".into()),
            timeout_secs: Some(5),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(
            outcome.error.is_none(),
            "the script did not run: {outcome:?}"
        );
        assert!(!outcome.success);
        assert!(outcome.stderr.len() <= MAX_STDERR_BYTES);
    }

    /// The deadline is real, not the script's own idea of one: `sleep 30` against
    /// a one-second allowance must come back as a timeout. Its output streams are
    /// redirected for the same reason as above — EOF, not the deadline, is what
    /// this test would otherwise be waiting on.
    #[tokio::test]
    async fn a_slow_script_is_killed_at_the_timeout() {
        let _slot = spawn_slot().await;
        let dir = sandbox("slow");
        write_script(&dir, "slow.sh", "#!/bin/sh\nsleep 30 >/dev/null 2>&1\n");

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("slow.sh".into()),
            timeout_secs: Some(1),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(!outcome.success);
        let error = outcome.error.clone().unwrap_or_default();
        assert!(error.contains("timed out"), "{error:?}");
    }

    /// A script that prints a paragraph must not put an over-long frame on the air.
    ///
    /// The budget used to be enforced here, on the whole blob. It is enforced
    /// per line in `split_reply` instead, so that is what this asserts — the
    /// count is capped too, because 200 lines is 200 transmissions.
    #[tokio::test]
    async fn a_very_chatty_script_becomes_a_capped_number_of_frames() {
        let _slot = spawn_slot().await;
        let dir = sandbox("chatty");
        write_script(
            &dir,
            "loud.sh",
            "#!/bin/sh\nfor i in $(seq 1 200); do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done\n",
        );

        let ctx = parse::parse("alarm arm").unwrap();
        let action = ActionSpec {
            script: Some("loud.sh".into()),
            ..ActionSpec::default()
        };
        let outcome = run(&action, &ctx, &dir).await;
        assert!(outcome.success, "{outcome:?}");

        let replies = crate::verbs::split_reply(&outcome.stdout);
        assert_eq!(replies.len(), crate::verbs::MAX_REPLIES + 1, "{replies:?}");
        for reply in &replies {
            assert!(reply.len() <= MAX_REPLY_BYTES, "{} bytes", reply.len());
        }
        // The exact count depends on MAX_CAPTURE_BYTES and the line width, so
        // assert the shape rather than a number that is incidental to both.
        let last = replies.last().unwrap();
        assert!(
            last.starts_with('+') && last.ends_with(" more"),
            "overflow summary was {last:?}"
        );
    }
}
