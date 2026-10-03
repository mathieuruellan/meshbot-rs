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
Messages that resolve to an action now run the declared script and put its
replies on the air — one message per line of the reply. The config loader
(`src/config.rs`) is implemented too, so `config.example.yaml` is the real schema
and is validated by the test suite; the verb table, the channel map and the script
directory all come from `config.yaml`. It also owns the radio clock
(`set_radio_clock`) and polls the radio for liveness, redialling when it stops
answering (`run_connection` / `radio_is_alive`). Do not assume a feature exists
because it is described below — check `src/`.

## Build
```bash
cargo check
cargo clippy --all-targets
cargo fmt
cargo test          # 139 unit tests, no radio needed
cargo run          # needs MESHCORE_HOST/PORT reachable
```
There is still no test suite for the radio itself: the unit tests cover the
language, the latch, the config and the policy, and nothing covers a live
connection. The `script` tests do fork real children, so they are the only ones
that touch the process table: they take `SPAWN_SLOT` and run one at a time
(`src/script.rs`), because ten concurrent spawns across ten per-test runtimes is
how this module once failed on CI — `action failed` where a script's own failure
line was expected, and no way to tell a fork that failed from a test that
asserted the wrong thing.
`cargo run` needs a `config.yaml` (`MESHBOT_CONFIG`, default
`/data/meshcore/meshbot-rs/config.yaml`) and a reachable script directory — a
missing one is a hard startup error, not a warning. From a dev machine it will
then fail to connect unless a proxy is reachable, and a clean
`cannot connect to …` is expected, not a bug. To exercise the real verb table
without a deploy:

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

## Working on this repo (branch and workspace)
Do every change on a **fresh branch off an up-to-date `origin/main`**, in this
repository's own workspace/checkout — never on a long-lived branch, a branch
whose PR has already merged, or a checkout that has not fetched.

```bash
git fetch origin
git switch -c <type>/<short-topic> origin/main
# ... work, commit ...
git push -u origin <type>/<short-topic>    # open a PR into main
```

- **`origin/main` is the only base.** A local `main` (or any local branch) can be
  arbitrarily stale, and a branch that has already been merged is a dead end:
  committing on it builds on history that will never ship. Always
  `git fetch origin` first and branch from `origin/main` by name, not from
  whatever happens to be checked out.
- **This repository is its own workspace.** Work in its own checkout and its own
  session. Do not edit it from the `meshcore` deployment workspace, or vice
  versa: the two repos have separate histories, remotes and deploy triggers.
- **`main` is the default branch.** `feat:`/`fix:` commits reach it through a PR;
  release-please opens the release PR and publishes the versioned image (see
  [Deployment model](#deployment-model)). Do not commit straight to a branch
  that is already the base of an open or merged PR.

## Deployment model
Two repos, two triggers. This one publishes an image; it is never built by
`meshcore`.

```
feat:/fix: → main   →  CI          →  ghcr.io/…/meshbot-rs:latest
merge release PR     →  CI          →  …/meshbot-rs:0.3.0  ← Renovate pins this
push meshcore         →  Komodo     →  host pulls the pinned image
```

Note the second line: the released image is built by the **main push** that
merges the release PR, not by the tag push. That is deliberate, and it is the
opposite of what `on: push: tags` suggests.

**Why: the tag push never fires.** GitHub does not start workflow runs for
events created by `GITHUB_TOKEN`, and release-please creates its tag with
exactly that token. So `on: push: tags: ["v*"]` is dead code, `latest` and
`sha-<full>` move, the release *looks* like it worked, and the bare version the
host pins never appears. `0.2.0` is the proof: the release merged at 15:39 and
published only `main`/`latest`/`sha-…`; the `0.2.0` image appeared at 15:48
from a manual `gh workflow run ci.yml --ref v0.2.0`.

The fix is not a PAT — it is to stop depending on the tag push.
release-please *commits* `.release-please-manifest.json` in its release PR, so
the version is already in the tree of a main push that CI does see. The
"Resolve tags" step in `ci.yml` reads it and emits `type=raw,value=<version>`,
gated on that push having changed the manifest — ungated, every later merge
would re-tag the current version with newer code. No credential, nothing to
expire, and `type=semver` is kept so a hand-dispatched tag run still works.

**If you ever see a release with no versioned image**, look for a `push` run on
`main` whose commit changed the manifest. If that run is missing or red, the
`checks` job is what stopped it; rebuild the tag by hand:

```bash
gh workflow run ci.yml --ref v0.3.0
```

`workflow_dispatch` is already a trigger, and `type=semver` finds the version
on the ref, so this produces the missing tag from the tagged commit. Do **not**
work around it by pinning the host to `latest`: that moves on every merge.
- `meshcore/compose.yaml` pins a **tag**; there is no `build:` anywhere.
- Runtime config is **tracked in the meshcore repo** (`mathieu/meshcore`), mounted
  read-only from the stack directory. Editing `config.yaml` or `scripts/` there
  and pushing is a reviewed commit and a deploy — it requires no rebuild.
- `.env` is the one host-owned file, mounted read-only from
  `/data/meshcore/meshbot-rs/.env`, and stays gitignored in that repo.
- `latest` moves on **every** merge to `main` and is not a release. The pinned
  tag only moves when a release PR is merged. Those are two separate deploy
  paths; the unpinned one is for testing a change, the pinned one is the host.

## The radio clock
The bot owns it. `set_radio_clock()` runs on every successful **(re)connect**,
reads the device time with `GET_DEVICE_TIME`, and writes `SET_DEVICE_TIME` **only
when the radio is behind the container clock** by more than 60s. A device that is
ahead was set by hand, and overwriting it would be the regression rather than the
fix.

The radio keeps no clock worth trusting: it loses time when powered down, and
that is what makes this a startup step rather than a nicety. A failure to read
or set the clock is logged and the run continues — a radio that will not answer
a time query must not stop the bot from answering messages.

Verify from the logs, never from the exit code:

```bash
docker logs meshcore-bot-rs 2>&1 | grep -E "Device time|Radio clock updated|connection lost"
```

`Device time: N, System time: M` must appear, followed by exactly one of
`Radio clock updated to: N` or `Device time is current or ahead - no update
needed`. Neither means the bot never reached the radio.

**A radio power-cycle re-syncs.** The proxy keeps client sockets open when the
radio disconnects — it flips an internal flag, drops commands with `Command
dropped: radio not connected`, and reconnects the serial port itself — so no TCP
connection is broken and nothing re-enters `set_radio_clock()` on its own. The
bot therefore polls the radio: `run_connection()` runs a read-only
`GET_DEVICE_TIME` every `PROBE_INTERVAL` (60s), and a probe that fails ends the
connection. `main()` redials after `RECONNECT_DELAY` (5s), and a fresh connect
re-runs `set_radio_clock()`. The clock is therefore stale for at most about a
probe interval, not until the next restart.

This is in-process, not a container restart: `main()` loops over
`run_connection()` and reconnects on any transport or radio failure. A
`verify_channels()` disagreement is the one exception — it is returned as
`Ended::Fatal`, because a radio layout that does not match `config.yaml` is not
something a redial can fix.

The latch is created **per connection**, so a reconnect disarms every pending
confirmation. A two-step action armed before a radio outage must be re-issued
after it.

**Known gap: a backgrounded script holds the reply past its own deadline.**
`script::run` kills the child at `timeout_secs`, then drains both pipes to EOF
before it returns — and a process the script started in the background inherits
both. So `nohup something &` keeps the action, and with it the bot's
one-message-at-a-time loop, waiting for that process to exit, however long that
takes: measured at the full lifetime of the grandchild, not the deadline. The
kill is a `SIGKILL` to the direct child only, so nothing else ends it. The two
timeout tests redirect their `sleep` to `/dev/null` for exactly this reason —
they measure the deadline, and waiting on EOF instead would make them 30-second
tests. The fix is either a process group (`process_group(0)` at spawn, `killpg`
at the deadline) or a bounded drain that keeps whatever was captured; undecided,
and no tracked script backgrounds anything.

## CI and releases
`.github/workflows/ci.yml` and `.github/workflows/release.yml` exist. Three
jobs in `ci.yml`:

| job | runs on | does |
|---|---|---|
| `commitlint` | pull requests | Conventional Commit title check |
| `checks` | every push and PR | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` |
| `image` | pushes to `main` and to `v*` tags | buildx build, push to GHCR, gated on `checks` |

Image tags: a push that is not a release → `main`, `sha-<full>`, `latest`; a
push that changed `.release-please-manifest.json` → those plus the bare version
(`0.3.0`); a `v*` tag push → the same via `type=semver`. The bare form is the
one Renovate bumps cleanly in compose. If compose pins `v0.3.0` instead, add
`type=semver,pattern=v{{version}}` to the tag list.

Because the tag push does not fire, the bare version arrives via the manifest
read described under [Deployment model](#deployment-model). The `image` job
therefore checks out with `fetch-depth: 0`; the default depth of 1 leaves no
`HEAD^` for the comparison.

**`checks` is deliberately unconditional**, including on tag pushes. A `needs:`
on a conditionally-skipped job skips its dependents, so making `checks`
tag-conditional would silently stop the released image from ever being built.

Version comes from the commit messages, via release-please:

| commit | bump | in the changelog |
|---|---|---|
| `fix:`, `perf:`, `revert:`, `chore:`, `ci:`, `docs:`, `test:`, `refactor:`, `build:`, `style:` | patch | only the non-hidden ones |
| `feat:` | minor | yes |
| `feat!:` or a `BREAKING CHANGE:` footer | major | yes |

Two consequences worth knowing before editing the workflows:

- **release-please never releases on its own.** It opens or updates a release
  PR; merging *that* is what creates the tag, the GitHub Release, and — via the
  manifest it commits — the versioned image. A `chore:` merge therefore queues a
  release PR with an empty changelog body — that is intended, not a bug. Do not
  merge it unless you want the version to move.
- **The `commitlint` job is the only thing stopping a non-conventional title
  from merging.** A squash-merge title that is not a Conventional Commit is
  dropped by release-please's commit splitter, so that work would move no
  version and appear in no changelog. Enable required status checks in the repo
  settings, or the job can be bypassed.

The toolchain is pinned three times and none of the three can read each other:
`rust-toolchain.toml` (`channel = "1.98.1"`, which also gives local builds the
same compiler CI uses), `ARG RUST_VERSION` in the Dockerfile (a minor, since
the tag is `rust:1.98-slim-bookworm`), and `dtolnay/rust-toolchain@1.98.1` in the
`checks` job. Bump them together — Renovate does it in one PR, see below. Current
stable Rust is 1.98.1; edition 2024 needs 1.85 or newer.

### Renovate
`renovate.json` at the repo root, run by `.github/workflows/renovate.yml` on
Mondays at 05:00 UTC and by hand with a `dry_run` input. It manages the cargo
dependencies and `Cargo.lock`, the GitHub Action pins, and the `debian` base
image.

- **`RENOVATE_TOKEN` is a PAT and it is the only secret in this repo.** Not
  `GITHUB_TOKEN`: Renovate opens PRs with it, and a PR opened by `GITHUB_TOKEN`
  starts no workflow run, so the `checks` and `commitlint` jobs would never see
  it. Autodiscover plus `RENOVATE_AUTODISCOVER_FILTER`, rather than
  `RENOVATE_REPOSITORIES`, because the token can see every repo on the account
  and the filter is what stops Renovate opening PRs anywhere else.
- **The Mend hosted app is deliberately not installed here.** concept2 runs both
  and its scheduled job never opens a PR — every PR on that repo is authored by
  `app/renovate`, at times that match no cron run. Copying the app as well would
  mean two Renovates racing for the same branches. One mover of files.
- **The toolchain is the exception to "one dependency, one PR".** Those three
  pins are three different dependency names, which Renovate cannot group on its
  own, so `renovate.json` gives all three a shared `groupName`. The result moves
  `1.98.1` → `1.99.0` and `1.98` → `1.99` in a single PR. It is deliberately
  *not* a regex customManager: the `rust-version` datasource returns only
  three-component versions, so it would either skip the Dockerfile's `1.98` or
  coerce it to `1.99.0`, which is not a real `rust:` tag.
- **`pest` and `pest_derive` are grouped for a hard reason.** `pest` pins
  `pest_derive` to an exact version, and `cargo build --locked` in the
  Dockerfile turns a lone bump into a build failure rather than a warning.
- **Nothing merges itself.** No `automerge`, so the commitlint job and a human
  are both still in the loop. A `debian` codename bump therefore arrives as a PR
  and needs the package list below re-validated.
- **A merged dependency PR is a patch release**, which is what produces the next
  bare version for the Renovate in the meshcore repo to pin. Do not merge one
  while a release PR is open: it gets swept into that release.
- PRs are authored by `mathieuruellan`, because the PAT pushes as the account
  owner. The `renovate/` branch prefix is the only tell that a PR is not a
  human's.

### The image, and what it deliberately does not do
Runtime is `debian:bookworm-slim` plus `bash`, `ca-certificates`, `curl`, `jq`,
`iputils-ping` and `netbase`. That list is not decoration: the executor calls
`env_clear()` and then adds only the names a verb entry declares, so **a script
has no `PATH` and every binary it calls has to exist in the image at an absolute
path.** Adding a tool to the image is the only way a script can reach it. A
script naming something that is not installed fails at message time, not at
build time.

Two things the Dockerfile pointedly does *not* do:

- **It does not create the script allowlist directory.** The config loader
  canonicalizes `bot.script_dir` and treats absence as a hard startup error, so
  an absent compose mount stops the bot loudly instead of leaving it answering
  every command with "action failed" while looking healthy. The loader also
  returns the directory it validated against, and that is the one scripts are
  executed from — a script cannot be checked in one directory and run in
  another.
- **It does not bake in `.env` or `config.yaml`.** `.dockerignore` excludes them,
  so a local token file cannot reach a published layer by accident.

`WORKDIR /data/meshcore/meshbot-rs` exists only so `dotenvy` finds the
host-mounted `.env`: it searches upward from the current directory, so with
`CWD=/` it would never look in `/data` at all. `main.rs` ignores the dotenv
error, so a wrong `WORKDIR` does not fail startup — it leaves every credential
missing until an action runs.

**The live config is not in this repo.** `config.example.yaml` and
`scripts.example/` here are the templates, and this repo's test suite is what
validates them against each other. The config the bot actually runs is
`config.yaml` and `scripts/` in `mathieu/meshcore`, version controlled there and
mounted into the container. Fix a script here, then copy it across; do not edit
a deployed change in only one of the two repos.

The container runs as uid 1000, so the host-mounted `.env` must be **readable by
that uid** — which is a question of ownership, not of mode:

```bash
sudo chown -R 1000:1000 /data/meshcore/meshbot-rs
```

Chown the directory and `0600` is correct, including for `.env`, which holds HA,
Komodo and Proxmox tokens. The alternative — leaving the files owned by another uid and
relaxing them to `0644` — also works, because a bind mount is read by the
container's uid rather than mapped to it, but it puts a world-readable copy of
every credential on the host. `scripts.example/README.md` installs the scripts
`0755`, since the bot executes them directly and they are tracked in a public
repository, so there is nothing to hide in them.

Only `.env` still needs this treatment. `config.yaml` and `scripts/` arrive from
the meshcore stack directory as root-owned bind mounts and are world-readable,
which is correct: they hold no secrets, and the scripts must be executable by
uid 1000, so their tracked mode is `100755`.

### First publish, by hand
A package published to GHCR with `GITHUB_TOKEN` is created **private**, and
Actions cannot change that. Until it is set to Public in the package settings,
`docker pull` from the host fails with `unauthorized`. The
`org.opencontainers.image.source` label in the Dockerfile is what links the
package to this repository, which is what lets the workflow keep pushing to it
on later runs; it grants permission inheritance, not visibility.

Only the web UI changes visibility — there is no supported REST endpoint for
it, and it is **one-way**: a public package cannot be made private again. The
flip can take a few minutes to propagate, and until it does an anonymous pull
answers `404`, not `401` — a `404` means "not visible", so do not read it as a
wrong package name without also checking the registry.

### ping needs a capability
Docker grants neither `CAP_NET_RAW` nor a widened `ping_group_range`, so without
one of them `ping` fails and `internet-status.sh` falls through to its `/dev/tcp`
branch. That branch is **bash-only** (`/dev/tcp` is not POSIX), so under dash
every target reports `down` — the check lies rather than erroring. Apply the
compose change in the same deploy as the image, not after:

```yaml
cap_add: [NET_RAW]    # or: sysctls: {net.ipv4.ping_group_range: "0 2147483647"}
```

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

## The one hard invariant: the radio's channel table is read-only
**mc-webui owns the channel table.** It is configured there, out of band, and this
service may only read it. There is no code path in this crate that creates,
renames, empties or removes a channel — and
`nothing_in_this_crate_writes_the_radio_channel_or_contact_table` (`src/main.rs`)
fails the build if one appears, because the guarantee is otherwise only a
convention. The whole channel map now comes from `bot.channels` in `config.yaml`.

- `SET_CHANNEL` is the **only** command that engages the proxy's channel
  virtualizer (`channel_virtualizer.py`), which remaps a client's indices onto
  allocator-chosen physical slots. A `SET_CHANNEL` anywhere in this codebase
  silently invalidates the channel map and can overwrite the radio's real
  channels.
- **A channel mc-webui configured that `bot.channels` does not declare is read
  once at startup, counted in one log line, and left exactly as it is** — not
  tidied, not emptied, not "adopted". Indices 0 and 1 exist for the phone app and
  the family. Reading is fine; writing never is. `slot_verdict()` returns
  `Unmonitored` for those slots whether or not they hold a channel, so there is no
  case where a slot we ignore becomes one we correct.
- Declaring a channel in `config.yaml` is an **assertion that it already exists**,
  never a request to create it. Two failures stop the run, and neither is fixed by
  writing to the radio:
  - the radio reports a different name for a declared index → re-layout, stop
  - the radio will not report a declared index at all → it is missing from
    mc-webui, stop and say so
  A declared index at or past `config::CHANNEL_SLOTS` is refused at load, because
  the startup readback covers `0..CHANNEL_SLOTS` and nothing beyond it: such a
  channel would be listened to and never checked.
- `verify_channels()` reads every slot back at startup and **fails closed**, so
  radio re-layout can't silently point verbs at the wrong channel. Keep that
  behaviour; do not soften it to a warning.

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
  host mapping became table data rather than something a message could reach, a
  message-supplied address would only reintroduce the target-selection hole the
  enum removes.
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
  `confirm: true` additionally requires the latch. stdout is clamped to one
  frame; **a failed action's last line of stderr is relayed**, so both streams
  are public and neither may carry a token, an id or a URL. `DELETE`-style "no
  retries" reasoning still applies: no action retries, mesh airtime is scarce.
- **The host map is the verb table's job, and a script arg is literal.** The
  mapping is declared per enum word as literals: `reboot myServer` runs
  `pve-reboot.sh myServer 112 qemu`. This is the reverse of the earlier design,
  where `map_word` lived in the script — reversed so one tracked script works on
  every install with no per-deployment edit. A literal is still operator-declared
  and unreachable from a message: the grammar has no way to express one, so the
  enum still does the work and `reboot somethingElse` fires nothing. The
  literals are placeholders in the tracked config because the repo is public; the
  real map goes in the gitignored `config.yaml`, which is the only file the
  loader reads.
- **The kind picks both the API path and the token**, which is why it travels as
  an argument: `.../qemu/{vmid}/status/reboot`, `.../lxc/{vmid}/status/reboot`,
  or `.../status/reboot` for the node itself, which has no vmid segment and takes
  `-`. Guests use a `VM.PowerMgmt` token; the node needs `Sys.PowerMgmt` and a
  second token (`PVE_TOKEN_HOST`), so each word's entry names the token it needs.
  One `PVE_ALLOW_REBOOT` arms all of them, guests and hypervisor alike. meshbot
  still never learns what `alpha` *is* — only which id and kind the operator
  filed under that word.
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
  action result. Unresolved placeholder ⇒ fail at config load, not at message
  time. There is deliberately no `{{text}}`: action arguments may only expand to
  values the verb table declares. `${ENV}` is not expanded either — `.env` is
  read verbatim, so a script reads a value from the environment via `env:`
  instead. A reply may also read the message metadata the engine injects:
  `{{hops}}`, `{{delay}}`, `{{snr}}`, `{{sender_timestamp}}` and `{{repeaters}}`.
- **Reply-only actions**: `script` is optional. With no script the action runs
  nothing and succeeds, rendering its `reply` from the context alone; the loader
  refuses `{{stdout}}` there, because no process could fill it. `ping` is the
  example.
- **Message metadata**: after parsing, `with_system` injects `hops` (the
  `path_len` the radio reported), `delay` (now minus the sender timestamp,
  clamped at zero so a sender whose clock is ahead reads `0`, not negative),
  `snr` when present, `sender_timestamp`, and `repeaters`. The repeater chain is
  **not** in `ChannelMessage`; it is read from the `LOG_DATA` RF log the radio
  pushes immediately before the fetched message, matched on hop count and a
  short freshness window. A missed or mismatched log yields `?` rather than a
  guess, and each hop resolves to a contact name when the radio has one and its
  hex id otherwise.
- **Replies**: a successful action produces **one message per line** of its
  rendered reply, capped at `MAX_REPLIES` (4) plus a `+N more` line — so a script
  listing eleven things never becomes eleven transmissions. A single line that
  does not fit one frame is **not** truncated: it is split on word boundaries
  into `i/n` fragments (`1/2 ...`, `2/2 ...`), and if it needs more than
  `MAX_REPLIES` fragments the last carries a `…+N more` marker. Blank lines are
  dropped, so an action that renders to nothing sends nothing rather than a blank
  frame. A **failed** action is always exactly one message: it never renders its
  template, so a half-finished reboot is never reported as done. `help` and
  `help <verb>` are built in and rendered from the verb table, so a verb cannot
  be added without documenting it.
- **The stdout clamp is per line, not per blob.** It used to be applied to the
  whole of stdout in `script::run`, which silently truncated a multi-line report
  mid-list. `verbs::split_reply` applies it per line at the point of becoming
  messages, so the executor hands the template the script's output intact.
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
  phone app. The guard test
  (`nothing_in_this_crate_writes_the_radio_channel_or_contact_table`) fails the
  build on `.set_channel(`, `.add_contact(`, `.remove_contact(` or
  `.set_flood_scope(` anywhere in `src/` — if you find yourself needing one,
  delete the test deliberately in the same commit, not quietly.
- **Do not have the bot create, rename or tidy a channel**, including one
  `bot.channels` does not declare and one it declares but the radio has not got.
  mc-webui sets channels up; this service asserts they are there and stops if they
  are not.
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
is documented in that README and declared in `config.example.yaml`, and both have
to move together. The loader checks the config side — a declared script must
exist in `script_dir`, and a declared `env:` name must be a real environment
name — but it cannot check the script's own argv contract, so a mismatch is a
runtime failure. `config.example.yaml` is validated by the test suite, which is
why a schema change and an example change are the same commit.

`pve-reboot.sh` is inert unless `PVE_ALLOW_REBOOT=1`, and it is the only
destructive template. Keep it that way: a copied script that can take a
hypervisor down is a hazard on its own, independent of the bot's latch.

## Related repo
The `meshcore` compose stack (a sibling checkout, Forgejo-hosted) is where this
gets deployed. Its AGENTS.md documents the radio's channel table, the
`channel-scheduler` job, and the mTLS/Traefik front — read it before changing
anything about how this service is reached.
