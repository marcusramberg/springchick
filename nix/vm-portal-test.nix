# Portal FileChooser fit test: does a file picker actually fit a phone screen?
#
# GTK's own file chooser has a widget minimum width well over the ~360 logical
# px a phone has. xdg-shell lets us *configure* a client narrower, but a client
# may ignore that size, and GTK does — it commits its minimum and runs off the
# screen edge, taking its action buttons with it. There is no protocol hint that
# forces it narrower; the fix is routing the FileChooser portal to an adaptive
# backend (phrosh, from xdg-desktop-portal-phosh — GTK4 + libadwaita, built for
# this width). nix/module.nix does that routing; this check proves it works.
#
# The oracle is the compositor's own `toplevel size ... oversize=<bool>` line,
# which compares a client's committed xdg window geometry against the logical
# size it was configured for. The test runs both halves so the assertion has
# teeth:
#   Phase A — GTK's own in-process chooser widget -> oversize=true. This is the
#             bug, reproduced.
#   Phase B — the same request through the portal -> oversize=false, because
#             xdg-desktop-portal hands it to phrosh.
# Each phase is asserted against journal lines emitted *after* that phase
# started (cursor file), so B cannot be satisfied by A's windows or vice versa.
#
# Build for the host arch:  nix build .#checks.aarch64-linux.vm-portal -L
{ self, pkgs }:

let
  inherit (import ./test-support.nix { inherit self pkgs; }) mkTest;

  # Same GI bundle trick as nix/vm-dialog-test.nix: one directory of typelibs so
  # a bare python3 + pygobject can drive GTK4 without wrapGAppsHook.
  giEnv = pkgs.buildEnv {
    name = "gtk4-gi-typelibs";
    paths = [
      pkgs.gtk4
      pkgs.glib.out
      pkgs.pango.out
      pkgs.gdk-pixbuf
      pkgs.graphene
      pkgs.harfbuzz
      pkgs.gobject-introspection
    ];
  };

  pythonEnv = pkgs.python3.withPackages (ps: [ ps.pygobject3 ]);

  # Maps a window, then opens GTK's own in-process file chooser — the wide one.
  #
  # Deliberately the deprecated GtkFileChooserDialog and not GtkFileDialog: the
  # newer async API hands off to the portal whenever one is available, and
  # GTK_USE_PORTAL=0 does not override that, so it would quietly test the same
  # path as phase B. GtkFileChooserDialog is the in-process widget by
  # construction, which is exactly the thing that does not fit.
  chooserPy = pkgs.writeText "gtk-chooser-demo.py" ''
    import gi
    gi.require_version("Gtk", "4.0")
    from gi.repository import Gtk, GLib, Gio

    def on_activate(app):
        win = Gtk.ApplicationWindow(application=app, title="chooser-parent")
        win.set_default_size(300, 400)
        win.present()

        def open_chooser():
            dlg = Gtk.FileChooserDialog(
                title="Pick a file",
                transient_for=win,
                action=Gtk.FileChooserAction.OPEN,
            )
            dlg.add_button("Cancel", Gtk.ResponseType.CANCEL)
            dlg.add_button("Open", Gtk.ResponseType.ACCEPT)
            dlg.present()
            return False

        # Delayed so the parent's own (fitting) geometry is logged first and the
        # chooser's line is unambiguous.
        GLib.timeout_add(3000, open_chooser)

    app = Gtk.Application(application_id="org.springchick.ChooserDemo")
    app.connect("activate", on_activate)
    app.run(None)
  '';

  # Asks the FileChooser portal to open a picker, then sits in a main loop.
  #
  # The loop is the point. OpenFile is asynchronous: it returns a Request object
  # path immediately and delivers the answer later as a Response signal, so a
  # one-shot `gdbus call` exits the moment it has the handle — the portal then
  # loses its peer, gives up ("Unable to send response through sender") and the
  # picker closes again before it has drawn anything worth measuring.
  portalPy = pkgs.writeText "portal-open-file.py" ''
    import gi
    from gi.repository import Gio, GLib

    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    proxy = Gio.DBusProxy.new_sync(
        bus, Gio.DBusProxyFlags.NONE, None,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.FileChooser",
        None,
    )
    # OpenFile(s parent_window, s title, a{sv} options) -> o handle
    reply = proxy.call_sync(
        "OpenFile",
        GLib.Variant("(ssa{sv})", ("", "Pick a file", {})),
        Gio.DBusCallFlags.NONE, -1, None,
    )
    print("request handle:", reply.unpack()[0], flush=True)
    GLib.MainLoop().run()
  '';

  portalOpenApp = pkgs.writeShellApplication {
    name = "portal-open-file";
    runtimeInputs = [ pythonEnv ];
    text = ''
      export GI_TYPELIB_PATH="${giEnv}/lib/girepository-1.0"
      exec python3 ${portalPy}
    '';
  };

  gtkChooserApp = pkgs.writeShellApplication {
    name = "gtk-chooser-demo";
    runtimeInputs = [ pythonEnv ];
    text = ''
      export GI_TYPELIB_PATH="${giEnv}/lib/girepository-1.0"
      # GtkFileChooserWidget reads org.gtk.gtk4.Settings.FileChooser at
      # construction and GLib aborts (SIGABRT) if the schema is missing — this
      # is not wrapped by wrapGAppsHook here, so point it at gtk4's schemas.
      export GSETTINGS_SCHEMA_DIR="${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}/glib-2.0/schemas"
      export GDK_BACKEND=wayland
      # Software renderer: llvmpipe only, same as the other VM checks.
      export GSK_RENDERER=cairo
      # Belt and braces for phase A: keep GTK off the portal wherever it does
      # honour this (the chooser widget itself is in-process regardless).
      export GTK_USE_PORTAL=0
      exec python3 ${chooserPy}
    '';
  };
in
mkTest {
  name = "springchick-portal";

  packages = [
    gtkChooserApp
    portalOpenApp
  ];

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds(
        "systemctl --user -M tester@.host is-active springchick.service", timeout=90
    )
    sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()

    UNIT = "journalctl -b _SYSTEMD_USER_UNIT=springchick.service"

    def since_now():
        """A journalctl --since bound of 'right now', so each phase only ever
        matches lines its own windows produced."""
        return machine.succeed("date '+%Y-%m-%d %H:%M:%S'").strip()

    def user_run(unit, cmd, extra=""):
        machine.succeed(
            f"systemd-run --user -M tester@.host --collect --unit={unit} "
            f"--setenv=WAYLAND_DISPLAY={sock} {extra} {cmd}"
        )

    # --- Phase A: reproduce the bug -----------------------------------------
    # GTK's own file chooser, portal bypassed. It ignores the narrow configure
    # and commits its widget minimum, so the compositor reports oversize=true.
    phase_a = since_now()
    user_run("gtk-chooser", "$(command -v gtk-chooser-demo)")

    machine.wait_until_succeeds(
        f"{UNIT} --since '{phase_a}' | grep -qE 'toplevel size .* oversize=true'",
        timeout=90,
    )
    machine.screenshot("01-gtk-chooser-oversize")

    # Take it away again so its windows cannot bleed into phase B.
    machine.succeed("systemctl --user -M tester@.host stop gtk-chooser.service || true")

    # --- Phase B: the portal path -------------------------------------------
    # Same user intent, routed through xdg-desktop-portal. nix/module.nix names
    # `phrosh` as the FileChooser impl, so this must map an adaptive picker.
    phase_b = since_now()

    # Precondition, asserted separately because getting it wrong fails every
    # portal call with UnknownMethod and nothing else says why:
    # xdg-desktop-portal selects a backend by matching XDG_CURRENT_DESKTOP
    # against portals.conf, and it reads that from the *user manager's*
    # environment — not from springchick.service's own unit environment.
    machine.succeed(
        "systemctl --user -M tester@.host show-environment | "
        "grep -qx 'XDG_CURRENT_DESKTOP=springchick'"
    )

    # Requests the picker and holds the bus connection open (see portalPy).
    user_run("portal-open", "$(command -v portal-open-file)")

    # Oracle 1: xdg-desktop-portal actually *activated* the phrosh backend, i.e.
    # the explicit FileChooser preference beat phrosh's `UseIn=phosh`. Asserted
    # on a live process, not on the bus name — `busctl list` also reports merely
    # activatable names, which would pass just from the .service file existing.
    machine.wait_until_succeeds("pgrep -f xdg-desktop-portal-phrosh", timeout=90)

    # ...and the call was answered with a Request handle rather than refused. An
    # unresolved FileChooser impl fails here with UnknownMethod, which is what
    # a portals.conf that never matched looks like from the client side.
    machine.wait_until_succeeds(
        "journalctl -b _SYSTEMD_USER_UNIT=portal-open.service | grep -q 'request handle: /'",
        timeout=90,
    )

    # Oracle 2: the picker it mapped fits the screen it was configured for.
    machine.wait_until_succeeds(
        f"{UNIT} --since '{phase_b}' | grep -qE 'toplevel size .* oversize=false'",
        timeout=90,
    )
    # ...and nothing in this phase overflowed.
    machine.fail(f"{UNIT} --since '{phase_b}' | grep -qE 'toplevel size .* oversize=true'")
    machine.screenshot("02-portal-chooser-fits")

    machine.fail(
        "journalctl -b | grep -iE 'panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault'"
    )
  '';
}
