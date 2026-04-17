#!/bin/bash
#
# Install Flowstate
# Double-click this to install Flowstate and bypass macOS Gatekeeper.
#
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_NAME="flowstate.app"
APP_SRC="$SCRIPT_DIR/$APP_NAME"
APP_DEST="/Applications/$APP_NAME"

if [ ! -d "$APP_SRC" ]; then
    echo "Error: $APP_NAME not found next to this script."
    echo "Make sure you're running this from inside the disk image."
    read -n 1 -s -r -p "Press any key to close..."
    exit 1
fi

echo "==> Installing Flowstate to /Applications..."

if [ -d "$APP_DEST" ]; then
    echo "    Removing previous installation..."
    rm -rf "$APP_DEST"
fi

cp -R "$APP_SRC" "$APP_DEST"

echo "==> Clearing quarantine attributes..."
xattr -cr "$APP_DEST"

echo "==> Done! Launching Flowstate..."
open "$APP_DEST"
