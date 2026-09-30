# App library: a fresh install has an empty home page plus the library;
# swiping past the last page reaches it; a folder tile opens its panel; a
# member launches, tagged with the launch's app id. Opening a folder keeps
# Home as Home, so this asserts on the `folder opened` log line.
#
# Run:  nix build .#checks.aarch64-linux.vm-library -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest phone;

  # A category nothing else declares. foot's entries are System, so the
  # folders are exactly [Game, System].
  gameApp =
    name:
    pkgs.makeDesktopItem {
      inherit name;
      desktopName = name;
      categories = [ "Game" ];
      exec = "${pkgs.foot}/bin/foot --app-id=${name} -e sleep 6000";
    };
in
mkTest {
  name = "springchick-library";

  # No `homePages`: the empty home is what's under test.
  packages = [
    pkgs.foot
    (gameApp "aaa")
    (gameApp "bbb")
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    IPC_SOCK = "/run/user/1000/springchick-ipc.sock"
    machine.wait_until_succeeds(f"ls {IPC_SOCK}", timeout=30)

    JOURNAL = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    def dbg(line):
        return machine.succeed(
            f"SPRINGCHICK_IPC_SOCK={IPC_SOCK} springchick ipc {line}"
        ).strip()

    def foot_count():
        return int(machine.succeed("pgrep -c -x foot || true").strip())

    # Mirrored from crates/sc-layout/src/lib.rs, as in vm-arrange. Open-folder
    # rows reuse the grid cells shifted below the title band.
    W = ${toString phone.width}
    H = ${toString phone.height}
    H_MARGIN, TOP_PAD = 0.04, 0.04
    BAR_H, DOCK_H, DOTS_H = 0.03, 0.10, 0.02
    COLS, ROWS = 4, 6
    ICON_FRAC, LABEL_FRAC = 0.62, 0.18
    TITLE_H = H * 0.035  # sc_layout::library::TITLE_H_FRAC

    CELL_W = W * (1 - 2 * H_MARGIN) / COLS
    CELL_H = H * (1 - BAR_H - DOCK_H - DOTS_H - TOP_PAD) / ROWS
    ICON = CELL_W * ICON_FRAC

    def col(i):
        return int(W * H_MARGIN + i * CELL_W + CELL_W / 2)

    def row(r):
        cell_y = H * TOP_PAD + r * CELL_H
        return int(cell_y + (CELL_H - ICON - CELL_H * LABEL_FRAC) / 2 + ICON / 2)

    def member(i):
        """Centre of an open folder's member i: a grid cell pushed down by the
        panel's title band."""
        return (col(i % COLS), int(row(i // COLS) + TITLE_H))

    # An empty home: tapping the first slot launches nothing.
    dbg(f"tap {col(0)} {row(0)}")
    dbg("settle 2000")
    machine.screenshot("01-empty-home")
    machine.fail(f"{JOURNAL} | grep -qF 'state changed to App'")
    assert foot_count() == 0, "a tap on empty home must not launch anything"

    # Two pages: the empty home and the library.
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'state changed to Home .*page_count: 2'", timeout=15
    )

    # Past the library's edge on distance alone (> PAGE_COMMIT_FRAC).
    dbg(f"swipe {int(W * 0.85)} {int(H * 0.5)} {int(W * 0.15)} {int(H * 0.5)} 400")
    dbg("settle 2000")
    machine.screenshot("02-library-page")

    # Game sorts before System, so it's tile 0.
    dbg(f"tap {col(0)} {row(0)}")
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'folder opened index=0 name=Game members=2'", timeout=15
    )
    dbg("settle 2000")
    machine.screenshot("03-folder-open")
    assert foot_count() == 0, "opening a folder must not launch anything"

    # Members sort by name, so slot 0 is aaa.
    mx, my = member(0)
    dbg(f"tap {mx} {my}")
    machine.wait_until_succeeds(f"{JOURNAL} | grep -qF 'state changed to App'", timeout=30)
    machine.wait_until_succeeds("pgrep -x foot", timeout=30)
    # Attributed from the launch before foot announces its own app_id.
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qE 'attributed to launch .*app_id=aaa'", timeout=15
    )
    machine.wait_until_succeeds(
        f"{JOURNAL} | grep -qF 'state changed to App {{ toplevel: 0, app_id: \"aaa\" }}'",
        timeout=15,
    )
    machine.screenshot("04-launched-from-folder")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
