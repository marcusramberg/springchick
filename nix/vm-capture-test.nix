# Screencopy (`ext-image-copy-capture-v1`) test on the real DRM backend.
#
# The oracle is a real screenshot tool, `grim`, run inside the session: it must
# produce a PNG of the output's size whose pixels match what is on screen. Two
# regressions this pins down, both of which made `grim` print "failed to copy
# output" with nothing in the compositor's log:
#   - the owned capture `Session` being dropped in `new_session` (smithay then
#     sends `stopped` and fails every frame the client asks for);
#   - shm capture buffers being rejected — grim only ever allocates shm, never
#     dmabuf, so a dmabuf-only capture path serves no screenshot tool at all.
#
# The "not blank" check is what proves pixels were actually blitted rather than
# an empty buffer handed back as a success.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-capture
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;
in
mkTest {
  name = "springchick-capture";

  packages = [
    pkgs.grim
    pkgs.foot
    # wlr-screencopy side: wf-recorder is the reason that protocol exists here,
    # and ffprobe is how the resulting file is checked.
    pkgs.wf-recorder
    pkgs.ffmpeg
  ];

  # The captured PNG is inspected pixel-wise on the driver side.
  extraPythonPackages = p: [ p.pillow ];

  testScript = ''
    import base64
    import io
    import struct

    from PIL import Image

    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    machine.wait_until_succeeds("ls /run/user/1000/springchick-*.lock", timeout=30)
    socket = machine.succeed(
        "basename $(ls /run/user/1000/springchick-*.lock) .lock"
    ).strip()

    def grim(dest):
        """Screenshot from inside the session, returned as raw PNG bytes."""
        machine.succeed(
            "systemd-run --user -M tester@.host --collect --wait "
            f"--setenv=WAYLAND_DISPLAY={socket} "
            f"${pkgs.grim}/bin/grim {dest}"
        )
        machine.succeed(f"test -s {dest}")
        return base64.b64decode(machine.succeed(f"base64 -w0 {dest}"))

    def thumb(img):
        """16x32 greyscale bytes — enough to compare two views of one screen."""
        return img.convert("L").resize((16, 32)).tobytes()

    def mean_abs_diff(a, b):
        return sum(abs(a[i] - b[i]) for i in range(len(a))) / len(a)

    # The oracle: grim's capture must match what QEMU sees in the framebuffer.
    # That is stronger than any "is it blank" heuristic — it also catches a
    # y-flipped, channel-swapped, or stale-frame capture, and it does not care
    # how much of the home screen happens to be lit.
    def assert_matches_framebuffer(png, name):
        machine.screenshot(name)
        shot = Image.open(f"{machine.out_dir}/{name}.png")
        capture = Image.open(io.BytesIO(png))
        assert capture.size == shot.size, f"captured {capture.size}, screen is {shot.size}"
        diff = mean_abs_diff(thumb(capture), thumb(shot))
        assert diff < 12, f"{name}: capture differs from the screen (mean |d| = {diff:.1f})"
        return capture

    # 1. Home screen. The shell is drawn by Skia into the same framebuffer, so a
    #    correct capture is also the only automated check that the Skia overlay
    #    survives the offscreen render path (a separate FBO from scanout).
    home_png = grim("/tmp/home.png")
    assert home_png[:8] == b"\x89PNG\r\n\x1a\n", "grim did not write a PNG"

    # PNG IHDR: the capture must be the full output, not a stub.
    width, height = struct.unpack(">II", home_png[16:24])
    assert (width, height) == (720, 1440), f"captured {width}x{height}, want 720x1440"

    home = assert_matches_framebuffer(home_png, "capture-home")

    # 2. With a client on screen — exercises the app composite passes, not just
    #    the Skia shell overlay.
    machine.succeed(
        "systemd-run --user -M tester@.host --collect "
        f"--setenv=WAYLAND_DISPLAY={socket} "
        "${pkgs.foot}/bin/foot -e sleep 60"
    )
    machine.wait_until_succeeds(
        "journalctl -b _SYSTEMD_USER_UNIT=springchick.service "
        "| grep -F 'toplevel app_id resolved' | grep -F 'app_id=foot'",
        timeout=30,
    )
    machine.sleep(3)
    app = assert_matches_framebuffer(grim("/tmp/app.png"), "capture-app")
    assert thumb(app) != thumb(home), "capture unchanged after launching an app"

    # 3. wlr-screencopy, via the tool it was added for. wf-recorder negotiates
    #    on its own (it asks for dmabuf first and falls back to the shm buffer
    #    we advertise), records with copy_with_damage, and muxes on SIGINT.
    # systemd-run gives the unit no PATH, so every binary is absolute; and
    # `timeout` always exits 124 after signalling, so the recording is judged by
    # the file it produced, not by the exit code.
    machine.succeed(
        "systemd-run --user -M tester@.host --collect --wait "
        f"--setenv=WAYLAND_DISPLAY={socket} "
        "${pkgs.bash}/bin/bash -c '"
        "${pkgs.coreutils}/bin/timeout -s INT 8 ${pkgs.wf-recorder}/bin/wf-recorder "
        "-c libx264 -x yuv420p -y -f /tmp/rec.mkv || true'"
    )
    machine.succeed("test -s /tmp/rec.mkv")
    probe = machine.succeed(
        "${pkgs.ffmpeg}/bin/ffprobe -v error -select_streams v:0 "
        "-count_frames -show_entries stream=width,height,nb_read_frames "
        "-of csv=p=0 /tmp/rec.mkv"
    ).strip()
    rec_width, rec_height, frames = (int(v) for v in probe.split(","))
    # h264 rounds odd dimensions down to even; the output is 720x1440 here, so
    # this is an exact check either way.
    assert (rec_width, rec_height) == (720, 1440), f"recorded {rec_width}x{rec_height}"
    assert frames > 0, "recording has no frames"

    # A repeat capture must still work: sessions are held for their client's
    # lifetime, and a stale one left behind would stop the next capture.
    grim("/tmp/again.png")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
    machine.fail(
        "journalctl -b _SYSTEMD_USER_UNIT=springchick.service | grep -F 'screencopy:'"
    )
  '';
}
