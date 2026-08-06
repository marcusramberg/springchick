#!/usr/bin/env bash
# Driver for running + driving springchick (the winit nested compositor) on a
# Linux wayland host (niri, sway, …), headlessly screenshottable via grim.
#
# Subcommands:
#   build              Compile sc-compositor WITHOUT launching. Do this first: a
#                      cold build can take several minutes and easily exceed an
#                      agent's command timeout. Run it in the background or with a
#                      long timeout, so the fast `up` never carries the build cost.
#   up                 Launch the compositor nested in the host session (builds
#                      first if needed — see `build`). BLOCKS until the frame loop
#                      is up. On a cold/unbuilt tree this can exceed the command
#                      timeout; a timeout here does NOT mean failure — see below.
#                      Refuses if an instance/socket already exists.
#   client [CMD...]    Launch a foot wayland client INSIDE springchick. Default
#                      command fills the terminal with a test pattern. Tracks PID.
#   send "tap X Y"     Send one debug-input line (down/move/up/tap/swipe/settle)
#                      to the compositor over its unix socket; prints the reply.
#   wake               Clear an idle-faded host screen (dms's fade-to-dpms
#                      overlay) so a capture shows real content, not black.
#   shot FILE          grim-screenshot to FILE. Wakes the screen first, and
#                      scopes to the output springchick is on when it can tell.
#   down               Kill the compositor + every client THIS driver launched,
#                      by PID, and remove springchick sockets. Never pkills.
#
# NEVER use `pkill foot` to clean up: the user's own terminal is usually a foot
# window. This driver only ever kills PIDs it recorded in $STATE.
set -u

UID_=$(id -u)
RUNTIME="/run/user/${UID_}"
STATE="/tmp/sc-driver"
LOG="${STATE}/compositor.log"
SOCK="${STATE}/debug.sock"
HOST_WL="${SPRINGCHICK_HOST_WL:-wayland-1}"
mkdir -p "$STATE"

repo_root() { git -C "$(dirname "$0")" rev-parse --show-toplevel; }

cmd_build() {
  local root; root=$(repo_root)
  echo "building sc-compositor (cold builds take minutes — run backgrounded or with a long timeout)…"
  ( cd "$root" && nix develop --command cargo build -p sc-compositor )
}

cmd_up() {
  if [ -e "${RUNTIME}/springchick-0" ]; then
    echo "refuse: ${RUNTIME}/springchick-0 exists — an instance is already running." >&2
    echo "        run '$0 down' first (a stale instance makes new clients bind the old binary)." >&2
    return 1
  fi
  local root; root=$(repo_root)
  # `cargo run` compiles first if the tree isn't built — on a cold tree this
  # blocks well past a typical command timeout. Run `$0 build` first (see top).
  echo "launching compositor (host WAYLAND_DISPLAY=${HOST_WL})…"
  ( cd "$root" && \
    WAYLAND_DISPLAY="$HOST_WL" SPRINGCHICK_DEBUG_SOCK="$SOCK" SPRINGCHICK_BACKEND=winit \
    nix develop --command cargo run -p sc-compositor >"$LOG" 2>&1 ) &
  echo $! > "${STATE}/launcher.pid"
  # Wait for the frame loop.
  for _ in $(seq 1 180); do
    if grep -q "entering frame loop" "$LOG" 2>/dev/null; then
      # Record the actual binary PID (newest match) for a clean, PID-safe kill.
      pgrep -nf "target/debug/springchick" > "${STATE}/compositor.pid"
      echo "ready. pid=$(cat "${STATE}/compositor.pid")  socket=${RUNTIME}/springchick-0"
      grep "actual output size" "$LOG" | tail -1 | sed 's/\x1b\[[0-9;]*m//g'
      return 0
    fi
    sleep 1
  done
  echo "timeout waiting for 'entering frame loop'. tail:" >&2
  tail -20 "$LOG" >&2
  echo "note: if the log ends mid-build, the compositor is still compiling — the" >&2
  echo "      launch is fine, just slow. Run '$0 build' first next time. Check" >&2
  echo "      'ls ${RUNTIME}/springchick-0' + 'grep \"entering frame loop\" ${LOG}'" >&2
  echo "      before assuming failure, then proceed to send/shot." >&2
  return 1
}

cmd_client() {
  local pattern='for i in $(seq 1 40); do printf "%02d ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789##\n" $i; done'
  local runcmd
  if [ "$#" -gt 0 ]; then runcmd="$*"; else runcmd="$pattern"; fi
  WAYLAND_DISPLAY=springchick-0 XDG_RUNTIME_DIR="$RUNTIME" \
    foot --hold sh -c "$runcmd" >"${STATE}/client.log" 2>&1 &
  local pid=$!
  echo "$pid" >> "${STATE}/clients.pids"
  echo "client pid=$pid"
}

cmd_send() {
  [ "$#" -ge 1 ] || { echo "usage: $0 send \"tap X Y\"" >&2; return 1; }
  python3 - "$SOCK" "$1" <<'PY'
import socket, sys
sock, line = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock)
s.sendall((line + "\n").encode())
print(s.recv(256).decode().strip())
PY
}

# Path to the running niri's IPC socket, or empty on a non-niri host.
niri_sock() {
  [ -n "${NIRI_SOCKET:-}" ] && { echo "$NIRI_SOCKET"; return; }
  ls "${RUNTIME}"/niri.*.sock 2>/dev/null | head -1
}

# Namespaces of the host's layer-shell surfaces (niri only).
host_layers() {
  local ns; ns=$(niri_sock); [ -n "$ns" ] || return 1
  NIRI_SOCKET="$ns" niri msg -j layers 2>/dev/null \
    | python3 -c 'import json,sys;[print(l["namespace"]) for l in json.load(sys.stdin)]' 2>/dev/null
}

# Wake a faded/idle host screen. dms (the user shell) covers the output with a
# black `dms:fade-to-dpms` OVERLAY layer before DPMS — grim then captures that
# overlay, not the springchick window, so shots come back solid black.
#
# `niri msg action power-on-monitors` does NOT clear it: the overlay is dms's,
# and dms only retracts it on real seat activity (ext-idle-notify resume). So
# synthesise one harmless keystroke (a bare Shift, no side effect on the focused
# app) through the virtual-keyboard protocol.
# Two independent wake levers, because two different things blank the screen:
#   - DPMS off (niri's own): cleared by `power-on-monitors`.
#   - dms's black `fade-to-dpms` overlay: only retracted on real seat activity,
#     so synthesise a bare Shift (no side effect on the focused app).
poke_seat() {
  local ns; ns=$(niri_sock)
  [ -n "$ns" ] && NIRI_SOCKET="$ns" niri msg action power-on-monitors >/dev/null 2>&1
  WAYLAND_DISPLAY="$HOST_WL" XDG_RUNTIME_DIR="$RUNTIME" \
    nix run nixpkgs#wtype -- -k Shift_L >/dev/null 2>&1
}

cmd_wake() {
  host_layers >/dev/null 2>&1 || {
    # Non-niri host: can't inspect layers, so just poke the seat blind — a bare
    # Shift is harmless whether or not the screen was faded.
    echo "wake: not a niri host — sending a Shift keystroke blind"
    poke_seat
    return 0
  }
  if ! host_layers | grep -q "fade-to-dpms"; then
    echo "wake: screen already awake"
    return 0
  fi
  echo "wake: fade-to-dpms overlay present — sending a Shift keystroke"
  for _ in 1 2 3; do
    poke_seat
    sleep 1
    host_layers | grep -q "fade-to-dpms" || { echo "wake: screen awake"; return 0; }
  done
  echo "wake: overlay still present — shot will likely be black" >&2
  return 1
}

# Name of the host output the springchick window is on (niri only).
springchick_output() {
  local ns; ns=$(niri_sock); [ -n "$ns" ] || return 1
  # Exported, not a per-command prefix: the python below shells out to niri again.
  export NIRI_SOCKET="$ns"
  niri msg -j windows 2>/dev/null | python3 -c '
import json,subprocess,sys,os
ws=json.load(sys.stdin)
w=next((w for w in ws if (w.get("title") or "")=="springchick"), None)
if not w: sys.exit(1)
sp=json.loads(subprocess.check_output(["niri","msg","-j","workspaces"]))
o=next((s["output"] for s in sp if s["id"]==w["workspace_id"]), None)
print(o or "", end="")
sys.exit(0 if o else 1)
' 2>/dev/null
}

cmd_shot() {
  local out="${1:-${STATE}/shot.png}"
  # An idle-faded host renders a black overlay over everything; wake it first or
  # the capture is worthless.
  cmd_wake >&2
  # Scope to the output springchick is on when we can work it out — a multi-head
  # full capture buries the nested window in the user's desktop.
  local o; o=$(springchick_output)
  grab() {
    if [ -n "$o" ]; then
      WAYLAND_DISPLAY="$HOST_WL" XDG_RUNTIME_DIR="$RUNTIME" nix run nixpkgs#grim -- -o "$o" "$1"
    else
      WAYLAND_DISPLAY="$HOST_WL" XDG_RUNTIME_DIR="$RUNTIME" nix run nixpkgs#grim -- "$1"
    fi
  }
  grab "$out"
  # Backstop for a blanked screen the layer check didn't catch (plain DPMS off,
  # a different shell's overlay). A solid-black PNG compresses to a few tens of
  # KB where real content runs into the megabytes — crude, but it costs nothing
  # and the retry is harmless when the screen really is that dark.
  local sz; sz=$(stat -c %s "$out" 2>/dev/null || echo 0)
  if [ "$sz" -lt 102400 ]; then
    echo "shot: capture is ${sz}B — suspiciously uniform, poking the seat and retrying" >&2
    poke_seat; sleep 1; grab "$out"
    sz=$(stat -c %s "$out" 2>/dev/null || echo 0)
  fi
  echo "wrote $out (${o:-all outputs}, ${sz}B)"
}

cmd_down() {
  if [ -f "${STATE}/clients.pids" ]; then
    while read -r p; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null && echo "killed client $p"; done < "${STATE}/clients.pids"
    rm -f "${STATE}/clients.pids"
  fi
  for f in compositor.pid launcher.pid; do
    if [ -f "${STATE}/$f" ]; then
      p=$(cat "${STATE}/$f"); [ -n "$p" ] && kill -9 "$p" 2>/dev/null && echo "killed $f $p"
      rm -f "${STATE}/$f"
    fi
  done
  sleep 2
  rm -f "${RUNTIME}"/springchick-* 2>/dev/null
  echo "sockets cleaned"
}

case "${1:-}" in
  build)  shift; cmd_build "$@" ;;
  up)     shift; cmd_up "$@" ;;
  client) shift; cmd_client "$@" ;;
  send)   shift; cmd_send "$@" ;;
  wake)   shift; cmd_wake "$@" ;;
  shot)   shift; cmd_shot "$@" ;;
  down)   shift; cmd_down "$@" ;;
  *) echo "usage: $0 {build|up|client [CMD...]|send \"tap X Y\"|wake|shot FILE|down}" >&2; exit 1 ;;
esac
