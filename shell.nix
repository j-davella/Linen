{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  # v0.2 owns its terminals through a PTY (portable-pty): tmux is no longer needed.
  nativeBuildInputs = with pkgs; [
    rustup
    pkg-config
  ];

  buildInputs = with pkgs; [
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
    openssl
  ];

  shellHook = ''
    export WEBKIT_DISABLE_COMPOSITING_MODE=1
    if ! rustup toolchain list 2>/dev/null | grep -q stable; then
      rustup default stable
    fi
  '';
}
