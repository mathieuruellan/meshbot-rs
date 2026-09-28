#!/bin/sh
# meshbot-rs action script: reboot a machine through the Proxmox VE API.
#
# THIS IS THE ONLY DESTRUCTIVE TEMPLATE. It does nothing unless
# PVE_ALLOW_REBOOT=1 is set in the bot's .env, so a fresh install cannot reboot
# production by accident. The bot's own confirmation latch is the first gate;
# this is the second, because that latch is remote-triggered code and a copied
# script that can take a hypervisor down is a hazard on its own.
#
# argv: <word>     one of the enum words the verb table declares
# env:  PVE_URL, PVE_NODE, PVE_TOKEN_GUEST, PVE_ALLOW_REBOOT
#                  PVE_TOKEN_GUEST holds the complete Authorization header
#                  value, e.g. PVEAPIToken=root@pam!meshbot=8f14e45f-ceea-
#                  467a-9d4f-1a2b3c4d5e6f. Build it in the Proxmox UI; do not
#                  concatenate it here.
#                  PVE_ALLOW_REBOOT is the arming switch, and the verb entry has
#                  to name it: the child's environment is emptied, so anything
#                  this table does not declare is invisible here.
#
# The word -> vmid/kind mapping lives HERE, not in meshbot-rs. That is the whole
# point of the split: the bot never learns what alpha is, so this file can
# change without a config edit and without redeploying the service.
#
# The ids below are PLACEHOLDERS. This repository is public; publishing which
# host is which vmid on which hypervisor type is reconnaissance. Replace them
# with your real map when you install this file.
#
# Requires: curl, jq
set -eu

[ "$#" -eq 1 ] || { echo "usage: pve-reboot.sh <word>" >&2; exit 2; }
: "${PVE_URL:?PVE_URL is not set}"
: "${PVE_NODE:?PVE_NODE is not set}"
: "${PVE_TOKEN_GUEST:?PVE_TOKEN_GUEST is not set}"

# PLACEHOLDER MAP — replace with your own before enabling.
# The words match the enum the verb table declares, so this file and
# config.yaml stay in step.
# kind: qemu | lxc | host. The kind picks the API path; a QEMU guest and an LXC
# container take different endpoints and the token privileges differ.
map_word() {
  case "$1" in
  alpha) echo "100 qemu" ;;
  beta) echo "101 qemu" ;;
  gamma) echo "102 lxc" ;;
  delta) echo "103 qemu" ;;
  komodo) echo "104 lxc" ;;
  pve) echo "0 host" ;;
  *) return 1 ;;
  esac
}

word="$1"
entry=$(map_word "$word") || {
  echo "no mapping for '${word}' — edit map_word in this file" >&2
  exit 2
}
# Deliberately unquoted word splitting: "vmid kind" is two fields.
# shellcheck disable=SC2086
set -- $entry
vmid="$1"
kind="$2"

case "$kind" in
qemu) path="qemu/${vmid}/status/reboot" ;;
lxc) path="lxc/${vmid}/status/reboot" ;;
host)
  # Refused on purpose. The host endpoint needs Sys.PowerMgmt, and the guest
  # token in PVE_TOKEN_GUEST does not carry it. Rather than send a request
  # that will 401 confusingly, say what is actually missing.
  echo "host reboot not wired: needs a Sys.PowerMgmt token" >&2
  exit 3
  ;;
*)
  echo "unknown kind '${kind}'" >&2
  exit 2
  ;;
esac

url="${PVE_URL%/}/api2/json/nodes/${PVE_NODE}/${path}"

if [ "${PVE_ALLOW_REBOOT:-0}" != "1" ]; then
  # Dry run. Report what would happen and stop, so installing this file is
  # safe and an operator can see the map is right before arming it.
  printf 'dry-run: %s -> %s\n' "$word" "$url"
  exit 0
fi

# The system trust store, or nothing: no -k. Skipping verification would make a
# reboot request authenticatable by anyone who can intercept the connection.
# PVE_CACERT is deliberately not a knob here — see the README on why.
curl_args="-fsS --max-time 10 -X POST -H Content-Type:application/json"

# The API returns a UPID, not a result. The reboot has only been *accepted* at
# this point.
upid=$(curl $curl_args -H "Authorization: ${PVE_TOKEN_GUEST}" "$url") || {
  echo "reboot request rejected by ${PVE_URL}" >&2
  exit 1
}

upid=$(printf '%s' "$upid" | jq -r '.data // empty')
[ -n "$upid" ] || { echo "no task id returned" >&2; exit 1; }

# Poll the task. PVE runs the reboot asynchronously, so returning immediately
# would report success for something that has not happened yet.
node_task="${PVE_NODE}/${upid}"
i=0
while [ "$i" -lt 10 ]; do
  sleep 1
  i=$((i + 1))
  status=$(curl $curl_args -G \
    -H "Authorization: ${PVE_TOKEN_GUEST}" \
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
