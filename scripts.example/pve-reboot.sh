#!/bin/sh
# meshbot-rs action script: reboot a machine through the Proxmox VE API.
#
# THIS IS THE ONLY DESTRUCTIVE TEMPLATE. It does nothing unless
# PVE_ALLOW_REBOOT=1 is set in the bot's .env, so a fresh install cannot reboot
# production by accident. The bot's own confirmation latch is the first gate;
# this is the second, because that latch is remote-triggered code and a copied
# script that can take a hypervisor down is a hazard on its own.
#
# argv: <word> <vmid> <kind>
#                  word  the enum word the verb table declares, i.e. the name
#                        the machine goes by on the mesh. It is a label, and it
#                        is the only thing from the message that reaches this
#                        script.
#                  vmid  the Proxmox guest id, or `-` when kind is `host` — the
#                        node endpoint has no vmid segment.
#                  kind  qemu | lxc | host. Picks the API path, and the token:
#                        a QEMU guest and an LXC container take different
#                        endpoints and need different privileges.
# env:  PVE_URL, PVE_NODE, PVE_TOKEN_GUEST, PVE_TOKEN_HOST, PVE_ALLOW_REBOOT
#                  PVE_TOKEN_GUEST holds the complete Authorization header
#                  value for a guest, e.g.
#                  PVEAPIToken=root@pam!meshbot=8f14e45f-ceea-467a-9d4f-
#                  1a2b3c4d5e6f, with VM.PowerMgmt on /vms/{vmid}. Build it in
#                  the Proxmox UI; do not concatenate it here.
#                  PVE_TOKEN_HOST is the same header value carrying
#                  Sys.PowerMgmt on /nodes/{node}, which is what the host
#                  endpoint needs and the guest token does not have.
#                  PVE_ALLOW_REBOOT is the arming switch, and the verb entry has
#                  to name it: the child's environment is emptied, so anything
#                  this table does not declare is invisible here.
#
# There is deliberately no word -> vmid map in this file. The mapping is
# declared per word in the verb table (config.yaml in a deployment), so this
# script is the same file on every install and a deployment never has to modify
# it. It is also why the message cannot steer at anything: it supplies the
# word, and the table supplies everything that decides which machine that is.
#
# BOTH streams are potentially on the air: stdout becomes the reply, and stderr
# becomes the reply when this script exits non-zero. Neither may carry a token,
# an id or a URL. The detail that would help an operator goes to stderr during
# a dry run — which exits 0, so stderr is logged and not sent — and to stdout
# never.
#
# Requires: curl, jq
set -eu

[ "$#" -eq 3 ] || { echo "usage: pve-reboot.sh <word> <vmid> <kind>" >&2; exit 2; }

# An explicit check rather than `${VAR:?}`, whose message carries a
# `script: line:` prefix that would end up on the air.
require() {
  # $1 is the value, $2 the name to name in the message. Never the other way
  # round: the name must not be read from the environment.
  [ -n "$1" ] || { echo "$2 is not set" >&2; exit 2; }
}

require "${PVE_URL:-}" PVE_URL
require "${PVE_NODE:-}" PVE_NODE

word="$1"
vmid="$2"
kind="$3"

case "$kind" in
qemu)
  case "$vmid" in
  [0-9][0-9][0-9]*) ;;
  *) echo "kind qemu needs a numeric vmid" >&2; exit 2 ;;
  esac
  path="qemu/${vmid}/status/reboot"
  require "${PVE_TOKEN_GUEST:-}" "PVE_TOKEN_GUEST (a guest needs VM.PowerMgmt)"
  token="$PVE_TOKEN_GUEST"
  ;;
lxc)
  case "$vmid" in
  [0-9][0-9][0-9]*) ;;
  *) echo "kind lxc needs a numeric vmid" >&2; exit 2 ;;
  esac
  path="lxc/${vmid}/status/reboot"
  require "${PVE_TOKEN_GUEST:-}" "PVE_TOKEN_GUEST (a guest needs VM.PowerMgmt)"
  token="$PVE_TOKEN_GUEST"
  ;;
host)
  # The node endpoint is /nodes/{node}/status/reboot: no vmid segment, so the
  # `-` above is discarded here rather than sent.
  [ "$vmid" = "-" ] || { echo "kind host takes '-' as the vmid" >&2; exit 2; }
  path="status/reboot"
  # Deliberately a separate token. The guest token does not carry
  # Sys.PowerMgmt, so naming the missing variable is more useful than a 401.
  require "${PVE_TOKEN_HOST:-}" "PVE_TOKEN_HOST (the host needs Sys.PowerMgmt)"
  token="$PVE_TOKEN_HOST"
  ;;
*)
  echo "unknown kind '${kind}' — expected qemu, lxc or host" >&2
  exit 2
  ;;
esac

url="${PVE_URL%/}/api2/json/nodes/${PVE_NODE}/${path}"

if [ "${PVE_ALLOW_REBOOT:-0}" != "1" ]; then
  # Dry run. Report what would happen and stop, so installing this file is
  # safe and an operator can see the map is right before arming it. The id and
  # the URL go to stderr: this exits 0, so stderr is logged and never relayed.
  printf 'dry-run: %s reboot not sent (set PVE_ALLOW_REBOOT=1)\n' "$word"
  printf 'would POST %s\n' "$url" >&2
  exit 0
fi

# The system trust store, or nothing: no -k. Skipping verification would make a
# reboot request authenticatable by anyone who can intercept the connection.
# PVE_CACERT is deliberately not a knob here — see the README on why.
# -S keeps curl's own diagnosis for a hand run, so this handler below has to stay
# the LAST thing written to stderr: the bot relays the last line, and only one.
curl_args="-fsS --max-time 10 -X POST -H Content-Type:application/json"

# The API returns a UPID, not a result. The reboot has only been *accepted* at
# this point.
upid=$(curl $curl_args -H "Authorization: ${token}" "$url") || {
  echo "reboot request rejected by the API" >&2
  exit 1
}

upid=$(printf '%s' "$upid" | jq -r '.data // empty')
[ -n "$upid" ] || { echo "no task id returned" >&2; exit 1; }

# Poll the task. PVE runs the reboot asynchronously, so returning immediately
# would report success for something that has not happened yet. The bot kills
# this script at its own timeout, so the loop is bounded well inside the
# action's timeout_secs and the last line is what a kill leaves behind.
node_task="${PVE_NODE}/${upid}"
i=0
while [ "$i" -lt 6 ]; do
  sleep 1
  i=$((i + 1))
  status=$(curl $curl_args -G \
    -H "Authorization: ${token}" \
    --data-urlencode "node=${node_task}" \
    "${PVE_URL%/}/api2/json/nodes/${PVE_NODE}/tasks/${upid}/status" 2>/dev/null) || continue

  exitstatus=$(printf '%s' "$status" | jq -r '.data.exitstatus // empty')
  case "$exitstatus" in
  OK)
    printf '%s rebooting\n' "$word"
    exit 0
    ;;
  "")
    # Still running.
    continue
    ;;
  *)
    printf '%s reboot failed: %s\n' "$word" "$exitstatus"
    exit 1
    ;;
  esac
done

printf '%s accepted, still running\n' "$word"
exit 0