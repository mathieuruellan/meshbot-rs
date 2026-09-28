# AGENTS.md

## What this is
`meshbot-rs` — a Rust service that reads MeshCore **channel** messages, parses
them, matches the result against a table of declared verbs, runs the script the
matching verb declares, and replies with the result.

It is a **separate repository** from the `meshcore` compose stack, deliberately:
this is standalone software, while `meshcore` is deployment config. It talks to
the radio only as a plain TCP client of `meshcore-proxy` (`proxy:5000`), the same
way mc-webui does.

**Status: the bot now answers and acts.** Connection, channel verification, the
grammar (`src/meshbot.pest`), the flat context with reserved-key poisoning
(`src/parse.rs`), the verb table with built-in `help` and typo suggestions
(`src/verbs.rs`), the script allowlist and executor (`src/script.rs`), the
confirmation latch (`src/latch.rs`) and the on-air reply path (`decide` /
`handle_message` / `send` in `src/main.rs`) are implemented and unit-tested.
Messages that resolve to an action now run the declared script and put one reply
on the air. **Still missing**: the config loader — `config.example.yaml` documents
a target schema that nothing reads yet, and `default_verbs()` hardcodes the same
vocabulary as a placeholder. Do not assume a feature exists because it is
described below — check `src/`.

## Build
```bash
cargo check
cargo clippy --all-targets
cargo fmt
cargo test          # 91 unit tests, no radio needed
cargo run          # needs MESHCORE_HOST/PORT reachable
```
There is still no test suite for the radio itself: the unit tests cover the
language, the latch and the policy, and nothing covers a live connection. Note
that `cargo run` also needs `MESHBOT_SCRIPT_DIR` to exist — a missing allowlist
directory is a hard startup error, not a warning. `cargo run` from a dev machine
will fail to connect unless a proxy is reachable — a clean `cannot connect to …`
is expected, not a bug. To exercise the real verb table without a deploy:

```bash
MESHBOT_SCRIPT_DIR=./scripts.example cargo run
```

## Repo
- Remote `git@github.com:mathieuruellan/meshbot-rs.git`, branch `main`, public.
- GitHub account is **`mathieuruellan`**, not `mathieu`. Published image path is
  `ghcr.io/mathieuruellan/meshbot-rs:<tag>`. Earlier notes saying `mathieu` are
  wrong.
- Crate, binary, image tag and compose service are all named `meshbot-rs`.
  Keep them in sync.
- The upstream dependency `meshcore-rs` keeps its own name — that is a
  different project on crates.io, not a stale rename.
- **The repository is public, so no real host, VM, domain or credential may
  appear in any tracked file** — code, tests, comments, config examples, scripts
  or docs. The enum words are `alpha`, `beta`, `gamma`, `delta`, `komodo`, `pve`;
  the vmid map in `scripts.example/pve-reboot.sh` is placeholders. URLs use
  `example.com`. This applies to test fixtures as much as to documentation.

## Deployment model
Two repos, two triggers. This one publishes an image; it is never built by
`meshcore`.

```
push meshbot-rs  →  GitHub Actions  →  ghcr.io/mathieuruellan/meshbot-rs:<tag>
                                            ↓  Renovate bumps the tag
push meshcore    →  Komodo           →  host pulls the pinned image
```
- `meshcore/compose.yaml` pins a **tag**; there is no `build:` anywhere.
- Runtime config is **host-owned**, mounted read-only from
  `/data/meshcore/meshbot-rs/{config.yaml,.env}`. Editing config and restarting
  the container requires no rebuild.
- `.env` and `config.yaml` are gitignored; only the `.example` files are tracked.
- **There is no CI workflow yet.** The GHCR publish step described above is the
  intended design, not something that exists.

## meshcore-rs 0.2.0 — trust the compiler, not the README
Pinned exactly (`=0.2.0`) because the crate is young (~2k downloads, 76%
documented) and its README is actively wrong. Verified by reading the vendored
source at `~/.cargo/registry/src/*/meshcore-rs-0.2.0/`. Every one of these cost a
compile error:

| README/docs claim | Reality |
|---|---|
| `send_chan_msg` | **`send_channel_msg(channel, msg, timestamp)`** |
| `SelfInfo.advert_name` | **`SelfInfo.name`** |
| `start_auto_message_fetching() -> Result` | returns **`()`**; no `?`/`.context()` |

Real API in use:
- `MeshCore::tcp(host, port) -> Result<MeshCore>`
- `meshcore.commands() -> &Arc<Mutex<CommandHandler>>` — lock, then call
- `send_appstart() -> Result<SelfInfo>`
- `get_channel(u8) -> Result<ChannelInfoData>` (`.channel_idx`, `.name`, `.secret`)
- `send_channel_msg(u8, &str, Option<u32>) -> Result<()>`
- `event_stream_filtered(EventType::ChannelMsgRecv) -> impl Stream<Item = MeshCoreEvent>`
- `MeshCoreEvent { event_type, payload, attributes }`
- `EventPayload::ChannelMessage(ChannelMessage)`
- `ChannelMessage { channel_idx, path_len, txt_type, sender_timestamp, text, snr: Option<f32> }`

**Use `event_stream_filtered`, not `subscribe`.** `subscribe()` takes
`F: Fn(MeshCoreEvent) + Send + Sync + 'static` — a *synchronous* callback, so an
`.await` on an HTTP action inside it will not compile. The stream is the only
primitive that allows async work per message, and the whole rule engine depends
on that.

Before changing any `meshcore-rs` call, read the vendored source rather than
trusting docs.rs or the README.

## The one hard invariant: never send `SET_CHANNEL`
`ChannelInfoData` proves `get_channel` is read-only, and the whole channel map in
`CHANNELS` (`src/main.rs:15`) depends on indices passing through untouched.

- `SET_CHANNEL` is the **only** command that engages the proxy's channel
  virtualizer (`channel_virtualizer.py`), which remaps a client's indices onto
  allocator-chosen physical slots. A `SET_CHANNEL` anywhere in this codebase
  silently invalidates the channel map and can overwrite the radio's real
  channels.
- Never write channel names or secrets from this service. The radio's channel
  table is configured out of band (see the `meshcore` repo's AGENTS.md restore
  procedure).
- `verify_channels()` reads slots `0..8` back at startup and **fails closed** on
  a name mismatch, so radio re-layout can't silently point rules at the wrong
  channel. Keep that behaviour; do not soften it to a warning.

## Known bug to fix before the rule engine: `message_id()` collides
`ChannelMessage::message_id()` (vendored `src/events.rs:363`) is:
```rust
bytes[0] = self.channel_idx;
bytes[4..8].copy_from_slice(&self.sender_timestamp.to_be_bytes());
```
Channel index plus sender timestamp. **Nothing else** — not the sender, not the
text. Two people posting different messages to the same channel in the same
second produce an identical id.

A naive dedupe set would swallow the second message as a "LoRa repeat". Intended
fix: key on `message_id()` **plus a hash of `text`**, which still collapses
genuine repeats (same sender, same second, same text) while keeping same-second
collisions apart. This is **undecided** — confirm the approach before
implementing.

Note `ContactMessage::message_id()` (line 329) is a *different* method that does
use `sender_prefix`. Don't conflate the two. Also note neither is a strong ID:
both are lossy hashes with a 1-second time quantum.

## Design decisions
Recorded so implementation doesn't relitigate them.
- **Grammar**: `verb [word] [key=value ...]`, via `pest`. The verb is the only
  positional argument, and a bare word must be one the verb's entry declares —
  it is a lookup key, never a target. That is what makes it impossible for a
  message to name a host, an entity or a URL. Spaces only as separators — reject
  `\n`, `\r`, `\t`, and input over 64 **bytes**. Typed slot values: int, float,
  bool, quoted string, word. Words exclude `/` to block traversal, and also `=`
  and quotes so a second slot can't hide inside a word. **IPv4 was dropped**: once
  host mapping moved into the scripts, a message-supplied address would only
  reintroduce the target-selection hole the enum removes.
- **Context** is a **flat key/value map**, not a struct: `verb`, the declared
  enum word as `target`, message slots, `channel`, `channel_idx`, `snr`,
  `sender_timestamp`, `.env` keys, and `status`.
- **Reserved keys**: `verb`, `confirm`, `channel`, `channel_idx`, `snr`,
  `sender_timestamp`, `status`, `stdout`, `target`, and every `.env` key. A
  message carrying a reserved key must not overwrite the real value — that rule
  is what stops a mesh message spoofing `channel=#admin`. A collision
  **poisons** the key: the real value survives and the consuming verb does not
  match. `canonical()` excludes every reserved key, so `reboot alpha delay=5
  channel_idx=3` cannot become a second latch entry.
- **The parser is verb-shaped, not sentence-shaped.** Any first word is a verb
  guess, so even `hi there` resolves to verb `hi` with target `there` and earns
  an "unknown verb" pointer. Silence happens only for input the parser rejects:
  empty, control characters, over 64 bytes, or a third bare word. Worth knowing
  before putting the bot on a channel that carries prose.
- **A trailing `ok` is not a context value.** `reboot alpha ok` is handled in
  `parse()` so the token never lands on top of `target`. Because the parser
  cannot see the verb table it accepts `ok` on *any* verb; `decide()` rejects it
  for a non-gated verb rather than silently running the command.
- **The latch is the only gate.** There is deliberately no `Resolution::NeedsConfirm`
  and no `gated()`: a gated action arrives as a plain `Fire` with
  `confirm: true`, and `decide()` arms or consumes. A second "held" path would
  be a way to reach a spawn that skips the latch.
- **`.env`** is gitignored, mounted `:ro`, and is the single source for both
  context values and `${ENV}` interpolation in config. Webhook IDs and tokens
  live here, never in git.
- **Actions**: run a **script**, not an HTTP call. This is the change from the
  earlier HTTP design, and the reason is the same as the enum: the verb declares
  *which* script, never *where* it points. A script path is resolved against an
  allowlist directory and executed directly, never through a shell, so a
  template value cannot become command injection. The subprocess gets an
  explicit `env` allowlist and must not inherit the bot's environment, or the
  channel secret lands in every script. `mutating: true` marks a state change;
  `confirm: true` additionally requires the latch. Script **stderr is never
  relayed** and stdout is clamped to one frame. `DELETE`-style "no retries"
  reasoning still applies: no action retries, mesh airtime is scarce.
- **Host/VM mapping is the script's job.** meshbot-rs never learns that
  `alpha` is VM 100 or that it is a QEMU guest rather than an LXC container.
  That mapping is private to the script and can change without a config edit.
  For PVE that means the API call is chosen per kind: `.../qemu/{vmid}/status/
  reboot`, `.../lxc/{vmid}/status/reboot`, or `.../status/reboot` for the host
  itself. Guests use a `VM.PowerMgmt` token; the host needs `Sys.PowerMgmt` and
  a second token, which is deferred — so `reboot pve` is declared but not
  wired.
- **Confirmation latch**: 30 seconds, single use, only for `reboot`. `garage`
  and `alarm` are idempotent on a private channel, so a second message buys
  nothing. The latch keys on the **canonical** command, so `reboot alpha` and
  `REBOOT  alpha` share one armed state — but the confirm prompt must echo the
  **text that was typed**, never the canonical word. Prompting `reboot alpha
  ok` in answer to `reboot delta` would reboot a different machine.
- **Typos get a pointer, not a rejection**: unknown input always answers with a
  nearest-match suggestion, using OSA distance (Levenshtein plus adjacent
  transposition) because swapping two characters is the commonest phone-keyboard
  slip and plain Levenshtein scores `opne` → `open` as 2, outside tolerance.
- **Reply size is bytes**: MeshCore caps a channel payload at 160 bytes and
  `send_channel_msg` appends without any length check, so an over-long reply
  **hangs on the radio** rather than truncating. Clamp at 150 bytes on a
  character boundary. The old `max_reply_chars: 200` was wrong in kind and in
  number.
- **Templates**: `{{slot}}` for a declared value, `{{stdout}}` for the clamped
  action result, `${ENV}` for environment. Unresolved placeholder ⇒ fail at
  config load, not at message time. There is deliberately no `{{text}}`: action
  arguments may only expand to values the verb table declares.
- **Replies**: exactly one per inbound message, truncated to one frame, and *at
  most* one — an action that renders to nothing sends nothing rather than a blank
  frame. A failed action never renders its template, so a half-finished reboot is
  never reported as done. `help` and `help <verb>` are built in and rendered from
  the verb table, so a verb cannot be added without documenting it.
- **Channel scope is enforced in `decide()`**, before resolution, so it covers
  every outcome and not just the ones that spawn. A verb declared for one channel
  answers `not on this channel` on the other. `help` is not in the table and so
  has no scope — otherwise there is no way to ask what is possible.
- **`decide()` is pure and takes no `MeshCore`.** All policy — parse, scope,
  resolution, latch — lives there, and `handle_message` is only the I/O around
  it. That is what makes the latch and the scope check testable on a machine
  with no radio; keep it that way rather than folding the policy back into the
  async path.
- **`kill_on_drop(true)` on the child.** `wait_with_output` consumes the `Child`,
  so dropping the future on timeout drops the child — and tokio's default is
  `kill_on_drop(false)`, which would leave a script running past its timeout.
- **Rules**: declarative `when` constraints only. No expression DSL, no `eval`,
  no regex over raw message text. Unknown config keys fail at load.
- `schema: 1` at the top of `config.yaml`; refuse to start on mismatch so a
  config edited ahead of a deploy fails loudly.

## Do not
- **Do not add `SET_CHANNEL`**, or any write to the radio's channel table or
  contact list. This service shares one radio with mc-webui, Home Assistant and a
  phone app.
- **Do not use `subscribe()`** for message handling — sync callback, see above.
- **Do not put secrets or real hostnames in any tracked file**, not even in a
  test fixture or a comment. The repo is **public** and its history is
  permanent. Enum words are `alpha`/`beta`/`gamma`/`delta`/`komodo`/`pve`, the
  vmid map is placeholder, and URLs are `example.com`.
- **Do not rename the crate without also updating** the image tag, the compose
  service, and the Renovate-managed pin in the `meshcore` repo.
- **Do not act on a channel outside `CHANNELS`.** The map is the listen set and
  the startup assertion; keep the two identical.
- **Do not introduce retries** on actions. Mesh airtime is scarce and a retry
  storm is worse than a missed action.
- **Do not reintroduce a second gating path.** If something needs to be held for
  confirmation, it sets `confirm: true` and goes through the latch.

## Action scripts
`scripts.example/` holds the tracked templates; the deployed copies live in the
gitignored `/scripts` directory. Copy, never symlink — a symlink into this
checkout turns a local edit into a repo change. Read
`scripts.example/README.md` before changing one: the argument and env contract
is duplicated in `default_verbs()` and in `config.example.yaml`, and all three
have to move together.

`pve-reboot.sh` is inert unless `PVE_ALLOW_REBOOT=1`, and it is the only
destructive template. Keep it that way: a copied script that can take a
hypervisor down is a hazard on its own, independent of the bot's latch.

## Related repo
The `meshcore` compose stack (a sibling checkout, Forgejo-hosted) is where this
gets deployed. Its AGENTS.md documents the radio's channel table, the
`channel-scheduler` job, and the mTLS/Traefik front — read it before changing
anything about how this service is reached.
