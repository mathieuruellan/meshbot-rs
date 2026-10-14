# meshbot-rs

A rule-and-action bot for MeshCore. It listens on configured mesh channels for
commands, decides what each one means from a data-only verb table, and either
answers or runs an allowlisted script — no shell, no `PATH`, and an explicit
environment allowlist per entry, so a message can name a command but never a
host.

It reaches the radio only as a TCP client of `meshcore-proxy`; it never opens a
serial device. Home Assistant, Komodo and Proxmox are reached from the scripts.

## The message language

`verb [word] [key=value ...]`, prefixed by the command marker (`!` by default):

| message | meaning |
|---|---|
| `!garage` | status query — the verb's `get` |
| `!garage open` | action, using a word the verb declares |
| `!reboot alpha ok` | the confirmation for exactly that command |
| `!ping Nantes-44` | a verb whose argument a configured regex must accept |

`ping` is the verb that takes free text, because it declares `arg_pattern`:

```yaml
- name: ping
  channel: "#bot"
  arg_pattern: "[*]|eu|fr|fr-44|nantes|nte|landreau"
  get:
    reply: "ping @{{sender}}: {{delay}}s, {{hops}} hops | {{repeaters}}"
```

The pattern is an unanchored search — a *contains* test — and is compiled
case-insensitively, so `!ping Nantes-44` and `!ping 44NTE` answer, while `!ping`
and `!ping bonjour` are **silent**: a shared channel is not a place to correct
somebody's word choice. The argument decides whether the verb fires and nothing
else — it never reaches a reply template or a script argument. A pattern that
does not compile stops the bot at startup rather than failing per message.

## Staying quiet on a channel

`bot.help_list_only` names channels where only the bare `help` answers —
indices, each of which must be one `bot.channels` listens on:

```yaml
bot:
  help_list_only: [5]   # #public in config.example.yaml
```

On a listed channel `help <verb>` and a misspelled topic are dropped, no reply
ever points at `help`, and a sentence that merely lacks the `!` marker is
treated as chat instead of earning the `commands start with '!'` nudge. An
unknown first word is dropped too, on every channel, list-only or not. The key
narrows the help surface rather than muting the channel: a verb belonging to
another one still answers `not on this channel`, since channel scope is checked
first.

A wrong argument to `ping` is a separate, channel-independent silence, owned by
`arg_pattern`: wherever `ping` is declared, `!ping` and `!ping bonjour`
transmit nothing and only `!ping Nantes-44` answers.

## Run it

```sh
docker run --rm \
  -e MESHCORE_HOST="<host running meshcore-proxy>" \
  -v "$PWD/config.yaml:/data/meshcore/meshbot-rs/config.yaml:ro" \
  -v "$PWD/scripts:/data/meshcore/meshbot-rs/scripts:ro" \
  ghcr.io/mathieuruellan/meshbot-rs:0.11.0
```

Copy `config.example.yaml` to `config.yaml` and `scripts.example/` to
`scripts/` first. A config error, a missing script directory or a verb whose
word is not declared are startup errors, not warnings. Copy `.env.example` to
`.env` and fill it in too: a missing one starts the bot anyway, and every verb
then answers `unreachable` when its action runs.

## Documentation

**`AGENTS.md` is the source of truth** — design decisions, the config schema,
the security model, deployment. This file is a landing page and is not kept in
step with it beyond this sentence.

- `config.example.yaml` — the real schema, validated by the test suite
- `scripts.example/README.md` — what a verb may ask a script to do
- `CHANGELOG.md` — releases
