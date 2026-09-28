#!/bin/sh
# meshbot-rs action script: read one Home Assistant entity's state.
#
# Installed at /data/meshcore/meshbot-rs/scripts/. The bot resolves the name
# against that directory and refuses anything else, so this file is reachable
# only because the operator put it there.
#
# argv: <entity_id>          e.g. alarm_control_panel.alarm
# env:  HA_URL, HA_TOKEN     both required, both declared in the verb table
#
# stdout becomes the channel reply verbatim, so keep it to one short line:
# a channel payload is capped at 160 bytes and an over-long reply hangs the
# radio rather than truncating.
#
# Requires: curl, jq
set -eu

[ "$#" -eq 1 ] || { echo "usage: ha-entity.sh <entity_id>" >&2; exit 2; }
: "${HA_URL:?HA_URL is not set}"
: "${HA_TOKEN:?HA_TOKEN is not set}"

entity="$1"

# -fsS: fail on HTTP error, show errors, no progress bar. The token goes in a
# header, never on the command line, so it cannot land in a process listing.
body=$(curl -fsS --max-time 5 \
  -H "Authorization: Bearer ${HA_TOKEN}" \
  -H "Content-Type: application/json" \
  "${HA_URL%/}/api/states/${entity}") || {
  echo "unreachable" >&2
  exit 1
}

state=$(printf '%s' "$body" | jq -r '.state // "unknown"')
name=$(printf '%s' "$body" | jq -r '.attributes.friendly_name // empty')

if [ -n "$name" ]; then
  printf '%s: %s\n' "$name" "$state"
else
  printf '%s\n' "$state"
fi
