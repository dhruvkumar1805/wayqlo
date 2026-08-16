#!/bin/bash
# Builds wayqlo, installs the binary, and wires it into whichever idle
# daemon it finds. Safe to re-run: every check is idempotent and every
# file it edits gets backed up first.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
HYPRIDLE_CONF="$HOME/.config/hypr/hypridle.conf"

backup() {
    cp "$1" "$1.bak-$(date +%s)"
}

echo "Building wayqlo..."
cd "$REPO_DIR"
cargo build --release

mkdir -p "$BIN_DIR"
install -m755 target/release/wayqlo "$BIN_DIR/wayqlo"
echo "Installed to $BIN_DIR/wayqlo"

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*) echo "Note: $BIN_DIR is not on your PATH. Add it in your shell config." ;;
esac

if [[ -f "$HYPRIDLE_CONF" ]]; then
    if grep -q "wayqlo" "$HYPRIDLE_CONF"; then
        echo "wayqlo is already wired into $HYPRIDLE_CONF, leaving it as is."
    elif grep -q "omarchy-launch-screensaver" "$HYPRIDLE_CONF"; then
        # Omarchy ships its own screensaver hook in hypridle.conf. Take
        # over that existing slot instead of adding a second, competing
        # listener that would fire alongside it.
        backup "$HYPRIDLE_CONF"
        sed -i 's#pidof hyprlock || omarchy-launch-screensaver#pidof hyprlock || wayqlo#' "$HYPRIDLE_CONF"
        echo "Replaced the default screensaver hook in $HYPRIDLE_CONF with wayqlo."
        echo "Backup saved alongside it."
    else
        backup "$HYPRIDLE_CONF"
        cat >>"$HYPRIDLE_CONF" <<'EOF'

listener {
    timeout = 300
    on-timeout = wayqlo
    on-resume = pkill wayqlo
}
EOF
        echo "Added a wayqlo listener to $HYPRIDLE_CONF."
        echo "Backup saved alongside it."
    fi
elif command -v swayidle >/dev/null 2>&1; then
    echo "swayidle found, but sway configs vary too much to edit automatically."
    echo "Add this to your sway config:"
    echo
    cat "$REPO_DIR/contrib/swayidle.sh"
else
    echo "No hypridle.conf or swayidle found. See contrib/ for manual setup."
fi
