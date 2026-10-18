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

    # Not to be confused with this module's own `config = lib.mkIf ...` output.
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

    panelTransform = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.enum [
          "normal"
          "90"
          "180"
          "270"
        ]
      );
      default = null;
      example = "270";
      description = ''
        Turn a sideways-mounted panel upright (wl_output transform). Null
        follows the kernel's `panel orientation` connector property, which
        already covers the Steam Deck; set this when that quirk is missing
        or wrong.
      '';
    };

    waybar.enable = lib.mkEnableOption "waybar with springchick's example config (extras/waybar)";
  };

  config = lib.mkIf cfg.enable {
    environment = {

      systemPackages = [ cfg.package ];

      sessionVariables.XDG_DATA_DIRS = [
        # mobi.phosh.FileSelector
        "${pkgs.xdg-desktop-portal-phosh}/share/gsettings-schemas/${pkgs.xdg-desktop-portal-phosh.name}"
        # org.gnome.desktop.*, read by libadwaita/GTK.
        "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}"
      ];
    };

    # Lists springchick in greeters.
    services.displayManager.sessionPackages = [ cfg.package ];

    # cpuset for `[resources].bg_allowed_cpus`. Delegate= replaces systemd's list
    # (`pids memory cpu`), so the defaults are repeated.
    systemd.services."user@".serviceConfig.Delegate = "cpu cpuset io memory pids";

    # A Type=notify user service that BindsTo graphical-session.target, so
    # READY pulls the target active (the niri model).
    systemd.user.services.springchick = {
      description = "springchick Wayland compositor";
      documentation = [ "https://github.com/marcusramberg/springchick" ];
      # Started by springchick-session only.
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
      # The default pins a stripped PATH on the unit, shadowing the login PATH
      # springchick-session imports. Same as niri.service.
      enableDefaultPath = false;
      environment = {
        SPRINGCHICK_BACKEND = "drm";
        XDG_CURRENT_DESKTOP = "springchick";
        XDG_SESSION_TYPE = "wayland";
      }
      // lib.optionalAttrs (cfg.panelTransform != null) {
        SPRINGCHICK_PANEL_TRANSFORM = cfg.panelTransform;
      };
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        Slice = "session.slice";
        ExecStart = "${cfg.package}/bin/springchick";
        Restart = "no";
        TimeoutStopSec = "10s";
        # Keep a thrashing app from stalling the render thread.
        CPUWeight = 200;
        MemoryMin = "128M";
      };
    };

    # Started by springchick-session after the compositor exits; conflicting with
    # graphical-session.target tears the session down (as niri-shutdown.target).
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

    # WantedBy the compositor, not graphical-session.target, so other
    # sessions (phosh) don't get it. After= a Type=notify unit waits for
    # READY, by which point WAYLAND_DISPLAY is in the user manager.
    systemd.user.services.springchick-waybar = lib.mkIf cfg.waybar.enable {
      description = "waybar for springchick";
      wantedBy = [ "springchick.service" ];
      partOf = [ "graphical-session.target" ];
      after = [ "springchick.service" ];
      # on-click commands (foot, springchick ipc) need the login PATH.
      enableDefaultPath = false;
      serviceConfig = {
        ExecStart = "${lib.getExe pkgs.waybar} -c ${../extras/waybar}/springchick.jsonc -s ${../extras/waybar}/springchick.css";
        Restart = "on-failure";
      };
    };

    fonts.packages = lib.mkIf cfg.waybar.enable [ pkgs.nerd-fonts.jetbrains-mono ];

    environment.etc."springchick/config.toml" = lib.mkIf (cfg.config != null) {
      text = cfg.config;
    };

    # .example so it never shadows the defaults or a user's config.toml.
    environment.etc."springchick/config.toml.example".source =
      "${cfg.package}/share/springchick/config.example.toml";

    hardware.graphics.enable = lib.mkDefault true;

    # `gpu_min_mhz`: the compositor (in `video`) raises the GPU clock floor while drawing.
    services.udev.extraRules = ''
      ACTION=="add|change", SUBSYSTEM=="devfreq", KERNEL=="*gpu*", RUN+="${pkgs.coreutils}/bin/chgrp video /sys%p/min_freq", RUN+="${pkgs.coreutils}/bin/chmod g+w /sys%p/min_freq"
    '';

    security.polkit.enable = lib.mkDefault true;

    # Matched by XDG_CURRENT_DESKTOP=springchick. Mirrors niri.nix.
    xdg.portal = {
      enable = true;
      extraPortals = [
        pkgs.xdg-desktop-portal-gnome
        # Only for its `phrosh` backend.
        pkgs.xdg-desktop-portal-phosh
      ];
      config.springchick = {
        default = [
          "gnome"
          "gtk"
        ];
        "org.freedesktop.impl.portal.Secret" = "gnome-keyring";
        # GTK's pickers are wider than a phone and ignore the configured size.
        # phrosh is adaptive. Named explicitly because its `UseIn=phosh` doesn't
        # match springchick (explicit preference overrides UseIn, portal ≥1.18).
        "org.freedesktop.impl.portal.FileChooser" = "phrosh";
        "org.freedesktop.impl.portal.AppChooser" = "phrosh";
      };
    };

    services.gnome.gnome-keyring.enable = lib.mkDefault true;

    # The keyring serves nothing until pam_gnome_keyring unlocks it, and the
    # gnome-keyring module only wires that into `login`, not greetd. Needs a
    # greetd that authenticates; autologin leaves it locked.
    # mkIf the whole attrset, or an empty greetd PAM service appears.
    security.pam.services = lib.mkIf config.services.greetd.enable {
      greetd.enableGnomeKeyring = lib.mkDefault true;
    };
  };
}
