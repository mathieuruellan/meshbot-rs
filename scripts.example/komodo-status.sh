#!/bin/sh
# meshbot-rs action script: Komodo fleet health.
#
# argv: none
# env:  KOMODO_URL, KOMODO_KEY, KOMODO_SECRET
#
# Reports the stacks and servers Komodo manages that are not in a good state,
# naming each one. That is what is worth knowing before rebooting something: a
# machine already in trouble will come back and then have its updates applied,
# which is not what someone asking for a restart expected.
#
# ONE MESSAGE PER LINE. The bot turns each line of stdout into its own channel
# message, so an all-clear is a single line and a fleet in trouble is one message
# per problem, numbered i/n. Past MAX_REPLIES (4) the bot stops and says
# "+N more", so the numbering stays honest about the true total instead of
# quietly renumbering a truncated list.
#
# Komodo auth is a key/secret PAIR on two headers, `X-Api-Key` and `X-Api-Secret`.
# It is not a bearer token and there is no `Authorization` header — that is
# Portainer's scheme and Komodo will not understand it. The server treats a key
# with no secret as an error rather than a downgrade, and looks the key up
# verbatim, so both values are sent unmodified. A key is all-or-nothing: it can
# do whatever its owning user can, so this belongs on a Read-only service user
# rather than an admin's key.
#
# Every endpoint here is POST with a JSON body. There is no GET. The response is
# the bare struct — NO `data` envelope — so the jq paths are `.name` and
# `.info.state`, never `.data.name`.
#
# Both list endpoints default to 30 per page and return NO pagination metadata,
# so `limit: 0` is the only way to see the whole fleet in one call. Without it, an
# unhealthy machine on page two reads as healthy and the answer is confidently
# wrong.
#
# `Disabled`, `Stopped`, `Paused` and `Deploying` are deliberate or transient and
# are not reported as problems. That is the same split Komodo's own
# GetStacksSummary draws by counting stopped/paused separately from unhealthy.
#
# stdout becomes one channel message per line, so keep each line short. Neither
# stream may carry the key, the secret or the URL.
#
# Requires: curl, jq
set -eu

# An explicit check rather than `${VAR:?}`, whose message carries a
# `script: line:` prefix that would end up on the air.
require() {
  [ -n "$1" ] || { echo "$2 is not set" >&2; exit 2; }
}

require "${KOMODO_URL:-}" KOMODO_URL
require "${KOMODO_KEY:-}" KOMODO_KEY
require "${KOMODO_SECRET:-}" KOMODO_SECRET

# $1 is the variant, e.g. ListServers. Both credentials are always required:
# `child_env_from` treats a missing allowlisted name as a hard error, so a verb
# cannot declare one "just in case" and have it fail at message time instead.
#
# Never `-L`: an oauth2-proxy in front of Komodo answers 302 with a login page,
# and following it would POST a Komodo API request at PocketID and hand back the
# HTML as if it were a result.
komodo_read() {
  curl -fsS --max-time 8 -X POST \
    -H 'Content-Type: application/json' \
    -H "X-Api-Key: ${KOMODO_KEY}" \
    -H "X-Api-Secret: ${KOMODO_SECRET}" \
    -d '{"limit":0}' \
    "${KOMODO_URL%/}/read/$1"
}

# $1 names the endpoint in the failure message. stderr is the reply when the run
# fails, so it must not carry a host, a path or a URL.
unreadable() {
  echo "cannot read $1" >&2
  exit 1
}

servers=$(komodo_read ListServers) || unreadable servers
stacks=$(komodo_read ListStacks) || unreadable stacks

# A bare array is what a successful ListServers returns. Anything else — an
# error document, or the HTML of a login page, which is what a 302 leaves behind
# — is not a fleet and must not be read as an empty, healthy one. `2>/dev/null`
# drops jq's own parse error: `unreadable` below is the diagnosis, and stderr's
# last line is what goes on the air.
[ "$(printf '%s' "$servers" | jq -r 'type' 2>/dev/null)" = array ] || unreadable servers
[ "$(printf '%s' "$stacks" | jq -r 'type' 2>/dev/null)" = array ] || unreadable stacks

# One filter over both lists. `--argjson` keeps the two payloads out of the
# filter text, so the predicates below read as what they are.
#
# ServerState is PascalCase ("Ok", "NotOk") and StackState is snake_case
# ("running", "unhealthy"); the two do not agree, which is the easiest thing to
# get wrong when matching them.
issues=$(jq -r -n \
  --argjson servers "$servers" \
  --argjson stacks "$stacks" '
    def bad_server: .info.state != "Ok" and .info.state != "Disabled";
    def bad_stack: (.info.state | test("^(running|stopped|paused|deploying)$") | not);
    ($servers[] | select(bad_server) | "server \(.name) \(.info.state)"),
    ($stacks[]  | select(bad_stack)  | "stack \(.name) \(.info.state)")
  ') || unreadable stacks

if [ -z "$issues" ]; then
  n=0
else
  n=$(printf '%s\n' "$issues" | wc -l)
fi

if [ "$n" -eq 0 ]; then
  # The all-clear still says what it looked at, so "healthy" is
  # distinguishable from a fleet with nothing registered in it.
  printf 'all ok: %s servers, %s stacks\n' \
    "$(printf '%s' "$servers" | jq 'length')" \
    "$(printf '%s' "$stacks" | jq 'length')"
  exit 0
fi

# `i/n` counts every problem, including the ones the bot will drop past its cap,
# so a truncated list is still readable as "1/9" rather than a confident "1/4".
i=0
printf '%s\n' "$issues" | while IFS= read -r issue; do
  [ -n "$issue" ] || continue
  i=$((i + 1))
  printf '%s/%s %s\n' "$i" "$n" "$issue"
done