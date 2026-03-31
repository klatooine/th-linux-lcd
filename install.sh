#!/bin/bash
set -e

BIN=target/release/th-linux-lcd
DEST=$HOME/.local/bin/th-linux-lcd

cargo build --release

mkdir -p "$HOME/.local/bin"

if [ -f "$DEST" ]; then
    cp "$BIN" "$DEST.new"
    mv "$DEST.new" "$DEST"
else
    cp "$BIN" "$DEST"
fi

# Systemd service
SERVICE_DIR="$HOME/.config/systemd/user"
mkdir -p "$SERVICE_DIR"
cp th-linux-lcd.service "$SERVICE_DIR/"
systemctl --user daemon-reload
systemctl --user enable th-linux-lcd

echo "Installed to $DEST"
echo "Service installed. Use: systemctl --user start th-linux-lcd"
