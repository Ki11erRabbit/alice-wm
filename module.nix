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

    # xdg-desktop-portal-wlr only implements the screen-capture portals
    # (ScreenCast/Screenshot) — it has no FileChooser implementation. With
    # only -wlr installed, any app that asks the portal for a file picker
    # (Flatpak apps, and native apps/browsers configured to use the portal
    # for their "Open"/"Save As" dialogs) gets no backend able to service
    # the request at all: the D-Bus call just fails, so no dialog window is
    # ever created for the compositor to show. xdg-desktop-portal-gtk
    # provides FileChooser (and the other generic portals); keep -wlr for
    # screen capture specifically, since -gtk doesn't implement that under
    # a non-GNOME wlroots-style compositor.
    xdg.portal = {
      enable = lib.mkDefault true;
      extraPortals = lib.mkDefault [ pkgs.xdg-desktop-portal-wlr pkgs.xdg-desktop-portal-gtk ];
      config.common = lib.mkDefault {
        default = [ "gtk" ];
        "org.freedesktop.impl.portal.ScreenCast" = [ "wlr" ];
        "org.freedesktop.impl.portal.Screenshot" = [ "wlr" ];
      };
    };

    security.polkit.enable = true;

    programs.uwsm.enable = lib.mkIf cfg.withUWSM true;
  };
}

