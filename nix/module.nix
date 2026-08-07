{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.springchick;
in
{
  options.programs.springchick = {
    enable = lib.mkEnableOption "springchick, a Springboard-style Wayland compositor";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.springchick;
      defaultText = lib.literalMD "`packages.<system>.springchick` from the springchick flake";
      description = "The springchick package to install.";
    };

    # Named `config`, not `keybindings`: config.toml is a general file (a
    # [keybinds] table today, more sections later), and `cfg.config` here is
    # unrelated to this module's own `config = lib.mkIf cfg.enable { ... }`
    # output attribute below — same name, two different things.
    config = lib.mkOption {
      type = lib.types.nullOr lib.types.lines;
      default = null;
      description = ''
        Raw TOML written to /etc/springchick/config.toml. See
        crates/sc-keys/src/config.rs for the [keybinds] table schema.
        Null (default) leaves the compositor's built-in defaults in
        place and does not touch /etc.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # GSettings schemas for the session's own helpers. nixpkgs installs schemas
    # under share/gsettings-schemas/<name>/glib-2.0/schemas, which is not on
    # XDG_DATA_DIRS by default — GUI apps normally get it baked in by
    # wrapGAppsHook, but xdg-desktop-portal-phosh's libexec binaries are
    # unwrapped ELFs and read the ambient environment. Without this the phrosh
    # backend aborts at startup ("No GSettings schemas are installed on the
    # system") the moment xdg-desktop-portal tries to activate it, which takes
    # the FileChooser portal down with it. Same idiom as nixos/modules/programs/
    # plotinus.nix.
    environment.sessionVariables.XDG_DATA_DIRS = [
      # mobi.phosh.FileSelector — the file selector's own settings.
      "${pkgs.xdg-desktop-portal-phosh}/share/gsettings-schemas/${pkgs.xdg-desktop-portal-phosh.name}"
      # org.gnome.desktop.{interface,privacy,sound,…} — read by libadwaita/GTK
      # for theme, fonts and animation preferences.
      "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}"
    ];

    # Puts share/wayland-sessions/springchick.desktop into the system profile so
    # greeters (greetd's regreet/gtkgreet, GDM, …) list springchick as a session.
    services.displayManager.sessionPackages = [ cfg.package ];

    # The compositor runs as a Type=notify user service rather than being exec'd
    # straight from the greeter. This is the niri model and the only correct way
    # to satisfy the graphical-session.target contract: the service
    # BindsTo=+Before= that target, so when the compositor sends sd_notify READY
    # the target is pulled active. graphical-session.target is RefuseManualStart,
    # so nothing may start it by hand — the binding is the sanctioned path. Once
    # active, xdg-desktop-portal-*, the OSK and anything else gating on a live
    # graphical session can finally start. nix/springchick-session drives this:
    # import-environment → start this service → force springchick-shutdown.target.
    systemd.user.services.springchick = {
      description = "springchick Wayland compositor";
      documentation = [ "https://github.com/marcusramberg/springchick" ];
      # Deliberately not WantedBy any target: it is started on demand by
      # springchick-session, never pulled up automatically at login.
      bindsTo = [ "graphical-session.target" ];
      before = [
        "graphical-session.target"
        "xdg-desktop-autostart.target"
      ];
      after = [ "graphical-session-pre.target" ];
      wants = [
        "graphical-session-pre.target"
        "xdg-desktop-autostart.target"
      ];
      # NixOS defaults this to true, which pins a stripped Environment=PATH=
      # (per-unit, highest precedence) onto the service and shadows the full
      # login PATH that springchick-session imports into the user manager.
      # With it off the compositor — and every app/shortcut it spawns —
      # inherits that imported login PATH. Same fix as niri.service upstream.
      enableDefaultPath = false;
      environment = {
        # Device backend (DRM/libseat), previously set on the session wrapper.
        SPRINGCHICK_BACKEND = "drm";
        XDG_CURRENT_DESKTOP = "springchick";
        XDG_SESSION_TYPE = "wayland";
      };
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        Slice = "session.slice";
        ExecStart = "${cfg.package}/bin/springchick";
        # If the compositor dies the session is over; do not respawn it.
        Restart = "no";
        # Compositor holds the DRM master + input; give it room to shut down.
        TimeoutStopSec = "10s";
      };
    };

    # Force-teardown target. springchick-session starts this after the
    # compositor exits; conflicting with graphical-session.target(-pre) stops
    # the whole session tree irreversibly, mirroring niri-shutdown.target.
    systemd.user.targets.springchick-shutdown = {
      description = "Shutdown running springchick session";
      unitConfig = {
        DefaultDependencies = false;
        StopWhenUnneeded = true;
      };
      conflicts = [
        "graphical-session.target"
        "graphical-session-pre.target"
      ];
      after = [
        "graphical-session.target"
        "graphical-session-pre.target"
      ];
    };

    environment.etc."springchick/config.toml" = lib.mkIf (cfg.config != null) {
      text = cfg.config;
    };

    # Reference config with every option at its default, for users to copy to
    # config.toml and edit. Kept as .example so it never shadows the built-in
    # defaults or a user's own /etc/springchick/config.toml.
    environment.etc."springchick/config.toml.example".source =
      "${cfg.package}/share/springchick/config.example.toml";

    # DRM master + libinput come from the logind seat the greeter hands over.
    hardware.graphics.enable = lib.mkDefault true;
    security.polkit.enable = lib.mkDefault true;

    # xdg-desktop-portal: needed for apps like Fractal (Matrix secrets portal),
    # file pickers, screenshots, etc. `config.springchick` writes
    # /etc/xdg/xdg-desktop-portal/springchick-portals.conf, matched by
    # XDG_CURRENT_DESKTOP=springchick set in the service environment.
    # Mirrors niri.nix upstream.
    xdg.portal = {
      enable = true;
      extraPortals = [
        pkgs.xdg-desktop-portal-gnome
        # Only for its `phrosh` backend (Account/AppChooser/FileChooser/
        # Wallpaper) — see the FileChooser note below. The sibling `phosh`
        # backend (Notification/Settings) is not selected anywhere here.
        pkgs.xdg-desktop-portal-phosh
      ];
      config.springchick = {
        default = [ "gnome" "gtk" ];
        # Secret portal only works with gnome backend (delegates to gnome-keyring).
        "org.freedesktop.impl.portal.Secret" = "gnome-keyring";
        # GNOME's/GTK's file and app pickers have a widget minimum width well
        # over the ~360 logical px a phone has, and a client may ignore the
        # narrower size we configure — so they run off the screen edge and
        # their action buttons become unreachable. phrosh (xdg-desktop-portal-
        # phosh's Rust backend) is GTK4 + libadwaita and adaptive, built for
        # exactly this width. Named explicitly because its .portal declares
        # `UseIn=phosh`, which XDG_CURRENT_DESKTOP=springchick does not match;
        # an explicit preference here overrides UseIn (xdg-desktop-portal ≥1.18).
        "org.freedesktop.impl.portal.FileChooser" = "phrosh";
        "org.freedesktop.impl.portal.AppChooser" = "phrosh";
      };
    };

    # Required for gnome-keyring Secret portal backend.
    services.gnome.gnome-keyring.enable = lib.mkDefault true;

    # …but enabling the daemon is not enough for it to be *usable*. The Secret
    # portal (and plain libsecret, which unsandboxed apps use directly) both end
    # up at org.freedesktop.secrets, which serves nothing until the login
    # keyring is unlocked. pam_gnome_keyring is what unlocks it, using the
    # password from the PAM stack that started the session — and the
    # gnome-keyring module wires that into `login` only. springchick sessions
    # come from a greeter, so on greetd the module never ran and every secret
    # lookup fails. GDM does this for itself; greetd does not.
    #
    # Note this can only work for a greetd that actually authenticates the user.
    # Under autologin (initial_session) there is no password to hand over, so
    # the keyring stays locked and gcr will prompt on first use instead.
    # mkIf wraps the whole attrset, not just the value: defining
    # `security.pam.services.greetd` at all would otherwise conjure an empty PAM
    # service named greetd on systems not using it.
    security.pam.services = lib.mkIf config.services.greetd.enable {
      greetd.enableGnomeKeyring = lib.mkDefault true;
    };
  };
}
