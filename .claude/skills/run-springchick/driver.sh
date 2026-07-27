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
#   shot FILE          grim-screenshot the whole host output to FILE.
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

cmd_shot() {
  local out="${1:-${STATE}/shot.png}"
  WAYLAND_DISPLAY="$HOST_WL" XDG_RUNTIME_DIR="$RUNTIME" nix run nixpkgs#grim -- "$out"
  echo "wrote $out"
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
  shot)   shift; cmd_shot "$@" ;;
  down)   shift; cmd_down "$@" ;;
  *) echo "usage: $0 {build|up|client [CMD...]|send \"tap X Y\"|shot FILE|down}" >&2; exit 1 ;;
esac
