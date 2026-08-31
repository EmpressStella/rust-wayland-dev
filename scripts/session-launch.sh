#!/bin/bash
set -euo pipefail

TARGET_DESKTOP="${1:-}"

if [[ -z "$TARGET_DESKTOP" ]]; then
  echo "Error: Missing session file argument." >&2
  exit 1
fi

if [[ ! -f "$TARGET_DESKTOP" ]]; then
  echo "Error: Upstream session $TARGET_DESKTOP not found." >&2
  exit 1
fi

if [[ "$TARGET_DESKTOP" != /usr/share/wayland-sessions/* ]]; then
  echo "Error: Refusing unsupported session path '$TARGET_DESKTOP'." >&2
  exit 1
fi

SESSION_NAME="$(basename -- "$TARGET_DESKTOP")"

case "$SESSION_NAME" in
*niri.desktop)
  unset WAYLAND_DISPLAY
  unset DISPLAY

  # The installer only creates this marker for the RHIT Dell Pro Max 16
  # Blackwell + AMD hybrid layout. Keep every other Niri session on its normal
  # automatic renderer selection.
  NIRI_RENDER_DEVICE_FILE="/etc/genoa/niri-render-drm-device"
  if [[ -r "$NIRI_RENDER_DEVICE_FILE" ]]; then
    NIRI_RENDER_DEVICE="$(<"$NIRI_RENDER_DEVICE_FILE")"
    NIRI_USER_CONFIG="${NIRI_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/niri/config.kdl}"

    if [[ -e "$NIRI_RENDER_DEVICE" && -r "$NIRI_USER_CONFIG" && -n "${XDG_RUNTIME_DIR:-}" ]]; then
      NIRI_WRAPPED_CONFIG="$XDG_RUNTIME_DIR/genoa-niri-config.kdl"
      umask 077
      {
        printf 'include "%s"\n' "$NIRI_USER_CONFIG"
        printf 'debug {\n    render-drm-device "%s"\n}\n' "$NIRI_RENDER_DEVICE"
      } > "$NIRI_WRAPPED_CONFIG"
      if /usr/bin/niri validate --config "$NIRI_WRAPPED_CONFIG" >/dev/null 2>&1; then
        exec /usr/bin/niri --config "$NIRI_WRAPPED_CONFIG"
      fi
    fi
  fi

  exec /usr/bin/niri
  ;;
*sway.desktop)
  unset WAYLAND_DISPLAY
  unset DISPLAY
  exec /usr/bin/sway
  ;;
*gnome-wayland.desktop | *gnome.desktop)
  unset WAYLAND_DISPLAY
  unset DISPLAY
  exec /usr/bin/gnome-session
  ;;
*)
  echo "Error: Unsupported session '$SESSION_NAME'." >&2
  exit 1
  ;;
esac
