#!/bin/sh
# meshbot-rs action script: call a Home Assistant service.
#
# argv: <domain.service> <entity_id>
#         e.g. cover.open_cover cover.garage
#              alarm_control_panel.alarm_arm_home alarm_control_panel.alarm
# env:  HA_URL, HA_TOKEN
#
# The service and entity arrive as arguments declared per enum word in the verb
# table, so there is no word-to-call mapping in here. This script is deliberately
# dumb: it does exactly what it is told, and the verb table is the only place
# that knows `open` means `cover.open_cover`.
#
# stdout is NOT the reply for this action — the verb declares a literal reply
# string, because "the API returned 200" is not a useful thing to say to someone
# standing at their garage door. Anything printed here goes to the bot's logs
# via stderr, never to a channel.
#
# Requires: curl, jq
set -eu

[ "$#" -eq 2 ] || {
  echo "usage: ha-service.sh <domain.service> <entity_id>" >&2
  exit 2
}
: "${HA_URL:?HA_URL is not set}"
: "${HA_TOKEN:?HA_TOKEN is not set}"

service="$1"
entity="$2"

payload=$(jq -nc --arg e "$entity" '{entity_id: $e}')

code=$(curl -fsS -o /dev/null -w '%{http_code}' --max-time 5 \
  -X POST \
  -H "Authorization: Bearer ${HA_TOKEN}" \
  -H "Content-Type: application/json" \
  -d "$payload" \
  "${HA_URL%/}/api/services/${service}") || {
  echo "call to ${service} failed" >&2
  exit 1
}

echo "called ${service} for ${entity}, http ${code}" >&2
exit 0
