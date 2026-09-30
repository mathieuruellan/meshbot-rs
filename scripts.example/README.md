# Action script templates

These are **templates**, not the deployed scripts. The real ones live on the host
at `/data/meshcore/meshbot-rs/scripts/`, which is gitignored. Nothing here is
executed unless you copy it there.

## Why a separate directory

The bot resolves a verb's script name against `SCRIPT_DIR` and refuses anything
that resolves outside it. Two consequences follow:

- A verb entry can only ever reach a file an operator put in that directory. It
  is an allowlist, not a sanitiser.
- The names are **flat**. No subdirectories: `../` and `nested/script.sh` are
  both rejected, so there is no traversal to reason about.

Point the bot at this directory to try things without a deploy:

```sh
MESHBOT_SCRIPT_DIR=./scripts.example cargo run
```

## Install

```sh
install -d -m 0755 /data/meshcore/meshbot-rs/scripts
install -m 0755 scripts.example/*.sh /data/meshcore/meshbot-rs/scripts/
```

Copy, do not symlink or bind-mount. A symlink back into this checkout turns a
local edit into a repo change, and a token pasted in for a quick test becomes a
commit — this repository is public.

## The contract

The verb table decides what each script gets. Renaming an argument or an
environment variable means editing `default_verbs()` in `src/verbs.rs` (and
`config.yaml` once the loader lands) in the same change.

| script | arguments | environment |
|---|---|---|
| `ha-entity.sh` | `<entity_id>` | `HA_URL`, `HA_TOKEN` |
| `ha-service.sh` | `<domain.service> <entity_id>` | `HA_URL`, `HA_TOKEN` |
| `internet-status.sh` | `<target>...` | none |
| `komodo-status.sh` | none | `KOMODO_URL`, `KOMODO_TOKEN` |
| `pve-reboot.sh` | `<word> <vmid> <kind>` | `PVE_URL`, `PVE_NODE`, `PVE_TOKEN_GUEST`, `PVE_TOKEN_HOST`, `PVE_ALLOW_REBOOT` |

Arguments are either `{{placeholders}}` that expand to a declared value, or
literals written in config. `pve-reboot.sh` takes both: `{{target}}` is the
word from the message, and the vmid and kind are literals — which is why the
same script file works on every install without a per-deployment edit.

Two rules the executor enforces, which these scripts rely on:

- **No shell.** Arguments reach `execve` as separate argv elements. There is no
  command line to escape, so a message value can never become a command.
- **The environment is emptied first**, then the allowlisted names are added.
  Without `PATH`, so quote any path you use, and expect nothing else to be set.

That second point is why these scripts use `#!/bin/sh` (an absolute path) and
absolute paths for `curl`, `jq` and `ping`. `command -v` still works because
`sh` has a built-in default PATH for lookups; if it does not in your shell, use
absolute paths.

The same point cuts the other way, and it is easy to get wrong: **a variable the
verb entry does not name is invisible to the script.** `PVE_ALLOW_REBOOT` is a
switch, not a credential, and still has to be in the `env` list — otherwise the
dry-run guard is permanently on and `reboot` silently never works. There is also
no `PVE_CACERT`: an optional variable cannot be expressed in an allowlist, so the
obvious knob would be a setting that silently does nothing. TLS uses the system
trust store.

## Output rules

stdout becomes the channel reply **verbatim** for any action whose reply template
is `{{stdout}}` — that is `ha-entity.sh`, `internet-status.sh`, `komodo-status.sh`
and `pve-reboot.sh`. Two consequences:

- **Keep it to one short line.** A channel payload caps at 160 bytes and the
  radio hangs on an over-long reply rather than truncating. The executor clamps
  at 150 bytes, so nothing is *lost*, but the answer gets cut off mid-word.
- **stdout is public.** `ha-service.sh` writes its detail to stderr instead,
  because its reply is a literal string from the verb table — and because a
  success never relays stderr (see below), so the detail stays in the log.

Never echo a token to stdout. `curl` gets it in a header, so it does not appear
in a process listing either.

### stderr is relayed on failure, so it is public too

This is the one rule that is easy to get wrong, because it is the opposite of
what it used to be. **stderr used to be discarded**; now the last non-empty line
of a failed action's stderr *is* the reply:

```
$ reboot alpha        →  confirm: 'reboot alpha ok'
$ reboot alpha ok     →  PVE_TOKEN_HOST (the host needs Sys.PowerMgmt) is not set
```

"action did not succeed" tells an operator nothing about a command they asked
for by name, and a mesh command has no terminal to look at. So:

- **Never put a token, a host id, a vmid or a URL on either stream.** The
  executor cannot tell a useful line from a secret one; keeping the streams safe
  is the script's job, and it is the same rule as for stdout.
- **On success, stderr is logged and never sent.** That is the escape hatch for
  detail: `pve-reboot.sh`'s dry run prints the request it would have sent to
  stderr and a safe one-liner to stdout, and exits 0.
- **A failed run never relays stdout.** Only stderr, and only its last line,
  clamped to 100 bytes. A partial result is never reported as an outcome.
- A script killed at its timeout keeps whatever it wrote first, so a poll loop
  that dies mid-wait can still say what it was doing.

## `pve-reboot.sh` is inert until you arm it

It prints what it would do and exits unless `PVE_ALLOW_REBOOT=1` is set:

```
$ MESHBOT_SCRIPT_DIR=./scripts.example ./scripts.example/pve-reboot.sh alpha 100 qemu
dry-run: alpha reboot not sent (set PVE_ALLOW_REBOOT=1)
would POST https://pve.example/api2/json/nodes/pve/qemu/100/status/reboot
```

The second line is stderr: a dry run exits 0, so it is logged and never relayed
onto a channel. That is the point of the arrangement — you can check the whole
map without publishing it.

Before enabling it, put your real map in `config.yaml`. One `args` entry per
machine, and the word is whatever you want to say on the air:

```yaml
- name: reboot
  channel: "#admin"
  args:
    - words: [myPersonalServer]
      script: pve-reboot.sh
      args: ["{{target}}", "112", "qemu"]
      env: [PVE_URL, PVE_NODE, PVE_TOKEN_GUEST, PVE_ALLOW_REBOOT]
      mutating: true
      confirm: true
      reply: "{{stdout}}"
```

The ids in `config.example.yaml` are placeholders (`alpha`, `beta`, …) because
publishing which host is which vmid on which hypervisor type is reconnaissance,
and this repository is public.

Three things about the mapping, which is deliberately the config's job and not
the script's:

- The **kind** picks the API path: `qemu/{vmid}/status/reboot` for a VM,
  `lxc/{vmid}/status/reboot` for a container, `status/reboot` for the node
  itself. It also picks the **token**, and that is why the kind travels as far
  as it does: a guest needs `VM.PowerMgmt` on `/vms/{vmid}` and the node needs
  `Sys.PowerMgmt`, which the guest token does not have. `PVE_TOKEN_HOST` is a
  second token from the PVE UI, named only by the entry that reboots the node.
- A **word you did not declare does nothing** — it answers
  `reboot: 'myServer'? did you mean: …?` and fires no script. The message
  chooses which declared entry runs; it can never supply an id.
- One switch arms everything. `PVE_ALLOW_REBOOT=1` covers the guests and the
  hypervisor alike, so treat arming it as the decision it is.

## Requirements

`curl` and `jq` on the host, plus `ping` for the internet check — or no `ping`
binary at all, in which case `internet-status.sh` falls back to a `/dev/tcp`
connect, which is unprivileged but only proves port 443 opens.
