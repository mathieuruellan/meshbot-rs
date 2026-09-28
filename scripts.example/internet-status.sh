#!/bin/sh
# meshbot-rs action script: is the internet actually reachable?
#
# argv: <target> [<target>...]   e.g. 8.8.8.8 1.1.1.1
# env:  none
#
# This runs inside the bot container, so it tests the container's view of the
# network, which is the thing that is actually broken when someone asks. It is
# NOT a test of the link out of delta: if this ever moves to the meshbot host
# the answer stops being about the host and becomes about whichever VM it is in.
#
# Raw sockets need CAP_NET_RAW, or a widened net.ipv4.ping_group_range, neither
# of which is granted by default. The /dev/tcp fallback works unprivileged and
# only proves a TCP port is open, which is a weaker but still useful signal.
#
# stdout becomes the channel reply verbatim, so keep it short.
#
# Requires: ping, or a POSIX shell (fallback path)
set -eu

[ "$#" -ge 1 ] || { echo "usage: internet-status.sh <target>..." >&2; exit 2; }

can_ping() {
  command -v ping >/dev/null 2>&1
}

ping_one() {
  target="$1"
  if can_ping; then
    # -c1 one probe, -W2 at most two seconds, quiet. A LoRa reply that takes
    # longer than the action timeout is not worth sending.
    if ping -c 1 -W 2 "$target" >/dev/null 2>&1; then
      printf '%s ok' "$target"
    else
      printf '%s down' "$target"
    fi
  else
    # No ping binary: fall back to a TCP connect. 443 is a reasonable stand-in
    # for "the internet is reachable" since DNS and routing both have to work
    # to get there.
    if timeout 2 sh -c "exec 3<>/dev/tcp/${target}/443" 2>/dev/null; then
      printf '%s ok' "$target"
    else
      printf '%s down' "$target"
    fi
  fi
}

out=""
for target in "$@"; do
  [ -n "$out" ] && out="${out} | "
  out="${out}$(ping_one "$target")"
done

printf '%s\n' "$out"
