{ flake }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.alice-wm;
in
{
  options.programs.alice-wm = {
    enable = lib.mkEnableOption "alice-wm, a Smithay-based Wayland compositor";

    package = lib.mkOption {
      type = lib.types.package;
      default = flake.packages.${pkgs.system}.alice-wm;
      defaultText = lib.literalExpression "alice-wm.packages.\${pkgs.system}.alice-wm";
      description = "The alice-wm package to install and register as a session.";
    };

    withUWSM = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Launch alice-wm through uwsm (Universal Wayland Session Manager)
        instead of directly. Gives you proper systemd session scoping,
        same as most other Smithay/wlroots compositors on NixOS.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # Registers the compositor as a login-manager session. This requires
    # the package to ship a share/wayland-sessions/alice-wm.desktop file
    # (see the stub in this template's build.rs / install step) — if you
    # don't need display-manager integration you can drop this line and
    # just launch alice-wm from a TTY.
    services.displayManager.sessionPackages = [ cfg.package ];

    # Most Smithay compositors talk to seatd for seat/session management
    # rather than going through logind directly.
    services.seatd.enable = lib.mkDefault true;

    # Needed for screen sharing / portals under wlroots-protocol compositors.
    xdg.portal = {
      enable = lib.mkDefault true;
      extraPortals = lib.mkDefault [ pkgs.xdg-desktop-portal-wlr ];
    };

    security.polkit.enable = true;

    programs.uwsm.enable = lib.mkIf cfg.withUWSM true;
  };
}

