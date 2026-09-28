#!/bin/sh
# meshbot-rs action script: Komodo edge/host maintenance state.
#
# argv: none
# env:  KOMODO_URL, KOMODO_TOKEN
#
# Komodo reports maintenance mode, which is the thing worth knowing before
# rebooting something: a host in maintenance will come back and then have its
# updates applied, which is not what someone asking for a restart expected.
#
# stdout becomes the channel reply verbatim, so keep it to one short line.
#
# Requires: curl, jq
set -eu

: "${KOMODO_URL:?KOMODO_URL is not set}"
: "${KOMODO_TOKEN:?KOMODO_TOKEN is not set}"

body=$(curl -fsS --max-time 5 \
  -H "Authorization: Bearer ${KOMODO_TOKEN}" \
  -H "Content-Type: application/json" \
  "${KOMODO_URL%/}/api/status") || {
  echo "unreachable" >&2
  exit 1
}

# `status` is RUNNING or MAINTENANCE. Anything else is worth surfacing verbatim
# rather than guessing at, so jq passes it through and the reply says what the
# API said.
status=$(printf '%s' "$body" | jq -r '.status // "unknown"')
version=$(printf '%s' "$body" | jq -r '.version // empty')
uptime=$(printf '%s' "$body" | jq -r '.uptime // empty')

if [ "$status" = "MAINTENANCE" ]; then
  printf 'MAINTENANCE%s\n' "${version:+ ${version}}"
else
  if [ -n "$uptime" ]; then
    printf '%s up %ss%s\n' "$status" "$uptime" \
      "${version:+ ${version}}"
  else
    printf '%s%s\n' "$status" "${version:+ ${version}}"
  fi
fi
