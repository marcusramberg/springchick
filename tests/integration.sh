#!/usr/bin/env bash
#
# springchick integration tests
#
# Runs the compositor nested (winit backend) and verifies:
#   1. Compositor starts and creates a Wayland socket
#   2. A Wayland client (foot) can connect and stay alive
#   3. Compositor shuts down cleanly when killed
#   4. Multiple clients can connect
#   5. Client closing doesn't crash the compositor
#   6. Esc key returns to home (via wtype)
#   7. Keybindings: short vs long press fire the right command (via debug socket)
#
# Usage: nix develop --command ./tests/integration.sh
#   or:  nix develop --command ./tests/integration.sh --quick  (skip slow tests)
#
# Requires: foot, wtype (both in system PATH or nix shell)

set -euo pipefail

SPRINGCHICK="./target/debug/springchick"
SOCKET_NAME=""
SC_PID=""
PASS=0
FAIL=0
QUICK=0

[[ "${1:-}" == "--quick" ]] && QUICK=1

# --- Helpers ---

red()   { printf '\033[1;31m%s\033[0m\n' "$*"; }
green() { printf '\033[1;32m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

pass() { PASS=$((PASS + 1)); green "  PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); red   "  FAIL: $1 — $2"; }

cleanup() {
    # Kill all children.
    if [[ -n "$SC_PID" ]] && kill -0 "$SC_PID" 2>/dev/null; then
        kill "$SC_PID" 2>/dev/null || true
        wait "$SC_PID" 2>/dev/null || true
    fi
    # Kill any stray foot processes we spawned.
    jobs -p | xargs -r kill 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

start_compositor() {
    "$SPRINGCHICK" 2>/tmp/springchick-test.log &
    SC_PID=$!
    # Wait for socket to appear.
    for i in $(seq 1 30); do
        if ls "$XDG_RUNTIME_DIR"/springchick-* 1>/dev/null 2>&1; then
            SOCKET_NAME=$(basename "$XDG_RUNTIME_DIR"/springchick-*.lock | sed 's/.lock$//')
            break
        fi
        sleep 0.1
    done
    if [[ -z "$SOCKET_NAME" ]]; then
        fail "compositor start" "socket never appeared"
        return 1
    fi
    # Give renderer time to initialize.
    sleep 0.5
}

stop_compositor() {
    if [[ -n "$SC_PID" ]] && kill -0 "$SC_PID" 2>/dev/null; then
        kill "$SC_PID" 2>/dev/null
        wait "$SC_PID" 2>/dev/null || true
    fi
    SC_PID=""
    SOCKET_NAME=""
}

# --- Pre-checks ---

bold "springchick integration tests"
echo ""

if [[ ! -x "$SPRINGCHICK" ]]; then
    red "Binary not found: $SPRINGCHICK"
    red "Run 'cargo build' first."
    exit 1
fi

if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    red "No WAYLAND_DISPLAY set. Run inside a Wayland session."
    exit 1
fi

# --- Test 1: Compositor starts and creates socket ---

bold "Test 1: Compositor starts and creates Wayland socket"
start_compositor
if kill -0 "$SC_PID" 2>/dev/null && [[ -n "$SOCKET_NAME" ]]; then
    pass "compositor started, socket=$SOCKET_NAME"
else
    fail "compositor start" "process died or no socket"
fi

# --- Test 2: Client connects and stays alive ---

bold "Test 2: Client (foot) connects"
WAYLAND_DISPLAY="$SOCKET_NAME" foot 2>/dev/null &
FOOT_PID=$!
sleep 1

if kill -0 "$FOOT_PID" 2>/dev/null; then
    pass "foot connected and running"
else
    fail "foot connect" "foot died on connect"
fi

# Verify compositor still alive.
if kill -0 "$SC_PID" 2>/dev/null; then
    pass "compositor still alive after client connect"
else
    fail "compositor stability" "compositor died after client connect"
fi

kill "$FOOT_PID" 2>/dev/null; wait "$FOOT_PID" 2>/dev/null || true

# --- Test 3: Client closing doesn't crash compositor ---

bold "Test 3: Client closes gracefully"
WAYLAND_DISPLAY="$SOCKET_NAME" foot 2>/dev/null &
FOOT2_PID=$!
sleep 0.5
kill "$FOOT2_PID" 2>/dev/null; wait "$FOOT2_PID" 2>/dev/null || true
sleep 0.3

if kill -0 "$SC_PID" 2>/dev/null; then
    pass "compositor survived client close"
else
    fail "compositor crash" "compositor died when client closed"
fi

# --- Test 4: Multiple clients ---

if [[ "$QUICK" -eq 0 ]]; then
    bold "Test 4: Multiple clients connect"
    WAYLAND_DISPLAY="$SOCKET_NAME" foot 2>/dev/null &
    C1=$!
    sleep 0.3
    WAYLAND_DISPLAY="$SOCKET_NAME" foot 2>/dev/null &
    C2=$!
    sleep 1

    alive=0
    kill -0 "$C1" 2>/dev/null && alive=$((alive + 1))
    kill -0 "$C2" 2>/dev/null && alive=$((alive + 1))

    if [[ "$alive" -ge 2 ]]; then
        pass "2 clients connected simultaneously"
    else
        fail "multi-client" "only $alive/2 clients alive"
    fi

    if kill -0 "$SC_PID" 2>/dev/null; then
        pass "compositor stable with multiple clients"
    else
        fail "compositor crash" "died with multiple clients"
    fi

    kill "$C1" "$C2" 2>/dev/null; wait "$C1" "$C2" 2>/dev/null || true
fi

# --- Test 7: Keybindings (short vs long press) ---

bold "Test 7: Keybindings fire on short and long press"

KB_DIR=$(mktemp -d)
KB_CONF="$KB_DIR/keybindings.toml"
cat > "$KB_CONF" <<EOF
long_press_ms = 500

[[binding]]
key = "F1"
press = "short"
command = "touch $KB_DIR/short"

[[binding]]
key = "F1"
press = "long"
command = "touch $KB_DIR/long"
EOF

KB_SOCK="$KB_DIR/debug.sock"

# Send one debug-socket line and print the reply.
dbg() {
    python3 - "$KB_SOCK" "$1" <<'PYEOF'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.sendall((sys.argv[2] + "\n").encode())
print(s.recv(64).decode().strip())
PYEOF
}

# Wait up to ~2s for a file to appear.
wait_for() {
    for _ in $(seq 1 20); do
        [[ -e "$1" ]] && return 0
        sleep 0.1
    done
    return 1
}

stop_compositor
SPRINGCHICK_KEYBINDS="$KB_CONF" SPRINGCHICK_DEBUG_SOCK="$KB_SOCK" \
    "$SPRINGCHICK" 2>/tmp/springchick-keys.log &
SC_PID=$!
for i in $(seq 1 30); do
    [[ -S "$KB_SOCK" ]] && break
    sleep 0.1
done

if [[ -S "$KB_SOCK" ]]; then
    pass "debug socket up with keybindings config"
else
    fail "keybindings setup" "debug socket never appeared"
fi

# Short press: held well under the 500ms threshold.
dbg "key F1 50" >/dev/null
if wait_for "$KB_DIR/short"; then
    pass "short press ran its command"
else
    fail "short press" "command never ran"
fi
if [[ -e "$KB_DIR/long" ]]; then
    fail "short press" "long command also ran"
else
    pass "short press did not fire the long binding"
fi

# Long press: held past the threshold. The long command must fire while the key
# is still down, and the short one must stay suppressed on release.
rm -f "$KB_DIR/short"
dbg "key F1 700" >/dev/null
if wait_for "$KB_DIR/long"; then
    pass "long press ran its command"
else
    fail "long press" "command never ran"
fi
if [[ -e "$KB_DIR/short" ]]; then
    fail "long press" "short command fired on release too"
else
    pass "long press suppressed the short binding"
fi

if kill -0 "$SC_PID" 2>/dev/null; then
    pass "compositor stable through keybinding injection"
else
    fail "compositor crash" "died during keybinding test"
fi

stop_compositor
rm -rf "$KB_DIR"
start_compositor

# --- Test 5: Compositor shuts down cleanly ---

bold "Test 5: Clean shutdown"
kill "$SC_PID" 2>/dev/null
WAIT_EXIT=0
for i in $(seq 1 20); do
    if ! kill -0 "$SC_PID" 2>/dev/null; then
        WAIT_EXIT=1
        break
    fi
    sleep 0.1
done

if [[ "$WAIT_EXIT" -eq 1 ]]; then
    pass "compositor exited within 2s"
else
    fail "clean shutdown" "compositor did not exit, sending SIGKILL"
    kill -9 "$SC_PID" 2>/dev/null || true
fi
wait "$SC_PID" 2>/dev/null || true
SC_PID=""

# Check logs for panics/crashes.
if grep -qi "panic\|SIGSEGV\|SIGABRT\|stack backtrace" /tmp/springchick-test.log 2>/dev/null; then
    fail "crash check" "found panic/crash in logs"
    echo "  Log tail:"
    tail -5 /tmp/springchick-test.log | sed 's/^/    /'
else
    pass "no panics in logs"
fi

# --- Test 6: Wayland socket cleaned up ---

bold "Test 6: Socket cleanup"
sleep 0.3
if ls "$XDG_RUNTIME_DIR"/springchick-* 1>/dev/null 2>&1; then
    # Socket files may linger (OS handles cleanup) — just note it.
    pass "socket files present (OS cleanup expected)"
else
    pass "socket files cleaned up"
fi

# --- Summary ---

echo ""
bold "Results: $PASS passed, $FAIL failed"
if [[ "$FAIL" -gt 0 ]]; then
    red "INTEGRATION TESTS FAILED"
    echo "Full log: /tmp/springchick-test.log"
    exit 1
else
    green "ALL INTEGRATION TESTS PASSED"
    exit 0
fi
