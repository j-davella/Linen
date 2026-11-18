{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    rustup
    pkg-config
    tmux
  ];

  buildInputs = with pkgs; [
    openssl
    webkitgtk_4_1
    gtk3
    libsoup_3
    glib
    cairo
    pango
    gdk-pixbuf
    librsvg
    at-spi2-atk
    at-spi2-core
    dbus
    glib-networking
    xdotool
  ];

  shellHook = ''
    export WEBKIT_DISABLE_COMPOSITING_MODE=1
    if ! rustup toolchain list 2>/dev/null | grep -q stable; then
      rustup default stable
    fi
  '';
}
