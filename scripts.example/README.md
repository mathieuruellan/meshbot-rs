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
| `pve-reboot.sh` | `<word>` | `PVE_URL`, `PVE_NODE`, `PVE_TOKEN_GUEST`, `PVE_ALLOW_REBOOT` |

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
- **stdout is public.** `ha-service.sh` writes to stderr instead, because the
  bot never relays stderr — its reply is a literal string from the verb table.

Never echo a token to stdout. `curl` gets it in a header, so it does not appear
in a process listing either.

## `pve-reboot.sh` is inert until you arm it

It prints what it would do and exits unless `PVE_ALLOW_REBOOT=1` is set:

```
$ MESHBOT_SCRIPT_DIR=./scripts.example ./scripts.example/pve-reboot.sh alpha
dry-run: alpha -> https://pve.example/api2/json/nodes/pve/qemu/100/status/reboot
```

Before enabling it, edit `map_word` in that file. The ids shipped here are
placeholders (`alpha`, `beta`, …) because publishing which host is which vmid on
which hypervisor type is reconnaissance, and this repository is public. The words
match the enum the verb table declares, so the two stay in step.

Two things to know about the mapping, which is deliberately the script's job and
not the bot's:

- The **kind** picks the API path: `qemu/{vmid}/status/reboot` for a VM,
  `lxc/{vmid}/status/reboot` for a container, `status/reboot` for the node
  itself.
- The **token** must match the kind. A guest needs `VM.PowerMgmt` on that
  `/vms/{vmid}`. The host needs `Sys.PowerMgmt` and a second token, so
  `reboot pve` is declared in the verb table but exits with a clear error rather
  than sending a request the guest token cannot authorise.

## Requirements

`curl` and `jq` on the host, plus `ping` for the internet check — or no `ping`
binary at all, in which case `internet-status.sh` falls back to a `/dev/tcp`
connect, which is unprivileged but only proves port 443 opens.
