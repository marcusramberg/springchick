# Arrange-mode (home-screen icon reorder) test.
#
# The one gesture path the other VM tests never reach, because it is the only
# one gated on *time* rather than motion: empty home background must be held
# past HOLD_MS (500ms) without moving into a swipe before arrange mode engages.
# (Holding an *icon* opens its context menu instead — see vm-icon-menu.)
# It exercises:
#   - long-press on empty background engages arrange mode (launches nothing);
#   - once in arrange, pressing an icon lifts it with no second hold, and
#     dragging it to another slot and releasing reorders the grid;
#   - the new order is persisted to state.toml, so it survives a restart;
#   - dragging an icon onto the dock pins it.
#
# Arrange mode changes no `UiState` discriminant (Home stays Home), so the
# `state changed to ...` trace log the other tests assert on never fires here.
# This test asserts on the `arrange engaged` / `arrange drop` trace logs and on
# the persisted state.toml instead.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-arrange -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;

  # Three catalog apps with names that sort deterministically, so the initial
  # grid order is known before the first gesture. They never need to run — the
  # test only reorders their icons — but the exec must be valid for the catalog
  # to accept the entry.
  gridApp =
    name:
    pkgs.makeDesktopItem {
      inherit name;
      desktopName = name;
      exec = "${pkgs.foot}/bin/foot --app-id=${name} -e sleep 6000";
    };
in
mkTest {
  name = "springchick-arrange";

  packages = [
    pkgs.foot
    # The test reads the persisted model back with tomllib.
    pkgs.python3
    (gridApp "aaa")
    (gridApp "bbb")
    (gridApp "ccc")
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"
    STATE = "/home/tester/.config/springchick/state.toml"

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    # Grid geometry in physical output pixels, mirrored from the layout
    # constants in crates/sc-layout/src/lib.rs. Eyeballed fractions are not good
    # enough here: an icon rect is only ~0.07H tall, so a press that misses it
    # reads as empty space and no long-press ever arms.
    W = ${toString phone.width}
    H = ${toString phone.height}
    H_MARGIN, TOP_PAD = 0.04, 0.04
    BAR_H, DOCK_H, DOTS_H = 0.03, 0.10, 0.02
    COLS, ROWS = 4, 6
    ICON_FRAC, LABEL_FRAC = 0.62, 0.18

    CELL_W = W * (1 - 2 * H_MARGIN) / COLS
    CELL_H = H * (1 - BAR_H - DOCK_H - DOTS_H - TOP_PAD) / ROWS
    ICON = CELL_W * ICON_FRAC

    def col(i):
        """Centre x of grid column i (0-based)."""
        return int(W * H_MARGIN + i * CELL_W + CELL_W / 2)

    def row(r):
        """Centre y of the icon in grid row r — not the cell centre: the icon
        sits above its label, so the two differ by half the label height."""
        cell_y = H * TOP_PAD + r * CELL_H
        return int(cell_y + (CELL_H - ICON - CELL_H * LABEL_FRAC) / 2 + ICON / 2)

    ROW0 = row(0)
    DOCK = int(H * 0.91)  # dock band spans 0.87H..0.97H

    def order():
        """The first page's app order, read straight from the persisted model."""
        # state.toml is only written after an arrange edit; before the first one
        # it may not exist at all.
        if machine.succeed(f"test -f {STATE} && echo y || echo n").strip() == "n":
            return None
        raw = machine.succeed(
            f"python3 -c \"import tomllib,sys;"
            f"print(' '.join(tomllib.load(open('{STATE}','rb'))['pages'][0]))\""
        ).strip()
        return raw.split()

    # Only three apps are installed, so every row below the first is empty
    # background — the surface the arrange long-press now lives on.
    EMPTY = row(4)

    # --- Long-press on empty background engages arrange mode ---
    # Hold still, well past HOLD_MS (500ms). `down` + sleep + `up` is the whole
    # point of this test: the hold is a *timer*, so the compositor must keep
    # advancing frames while a perfectly still finger rests on the screen.
    dbg(f"down {col(0)} {EMPTY}")
    machine.sleep(2)
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'arrange engaged'", timeout=15
    )
    dbg("up")
    machine.screenshot("01-arrange-engaged")

    # In arrange mode an icon is lifted by the press itself — no second hold.
    # Drag the first icon to the third slot of the first row and release. The
    # move is well past the tap slop, so this is a reorder, not a tap.
    dbg(f"down {col(0)} {ROW0}")
    dbg(f"move {col(1)} {ROW0}")
    dbg(f"move {col(2)} {ROW0}")
    dbg(f"move {col(2)} {ROW0 + 4}")
    dbg("up")
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'arrange drop .* action=Reorder'", timeout=15
    )
    machine.screenshot("02-after-reorder")

    # Neither the hold nor the drag may launch anything: arrange mode consumes
    # the pending launch. No toplevel ever mapped, so no App state was entered.
    machine.fail(f"{JOURNAL} | grep -qF 'state changed to App'")

    # --- The reorder landed in the model, and was persisted ---
    # First-run seeding orders the grid alphabetically by .desktop id, so page 0
    # starts [aaa, bbb, ccc, ...]. Dragging slot 0 to slot 2 must rotate exactly
    # those three and leave everything after them alone.
    after = order()
    assert after is not None, "state.toml was never written after the arrange edit"
    assert after[:3] == ["bbb", "ccc", "aaa"], (
        f"expected page 0 to start [bbb, ccc, aaa] after the drag, got {after[:6]}"
    )

    # --- Dragging onto the dock pins ---
    # Still in arrange mode (a drag release keeps it), so the press lifts.
    dbg(f"down {col(0)} {ROW0}")
    dbg(f"move {col(0)} {int(0.5 * H)}")
    dbg(f"move {col(1)} {DOCK}")
    dbg(f"move {col(1)} {DOCK + 2}")
    dbg("up")
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'arrange drop .* action=Pin'", timeout=15
    )
    machine.screenshot("03-after-pin")

    # The reorder above left bbb in slot 0, so that is the icon this second
    # drag picked up and dropped on the dock.
    pinned = machine.succeed(
        f"python3 -c \"import tomllib;"
        f"print(' '.join(tomllib.load(open('{STATE}','rb')).get('dock',[])))\""
    ).strip().split()
    assert pinned == ["bbb"], f"expected bbb pinned to the dock, dock={pinned}"
    # Pinning removes it from the grid — it lives in exactly one place.
    assert "bbb" not in order(), f"bbb is pinned but still on the grid: {order()[:6]}"

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
