#!/bin/bash
set -euo pipefail

# Keep the executable in its signed app bundle so authorization has Popsicle's icon.
app=${1:-/Applications/Popsicle.app}
binary="$app/Contents/MacOS/popsicle"
if [ ! -x "$binary" ]; then
    printf 'error: missing CLI: %s\n' "$binary" >&2
    exit 1
fi
app=$(cd "$app" && pwd -P)
destination="$HOME/.local/bin/popsicle"
mkdir -p "$HOME/.local/bin"
if [ -e "$destination" ] || [ -L "$destination" ]; then
    if [ "$(readlink "$destination" || true)" != "$app/Contents/MacOS/popsicle" ]; then
        printf 'error: refusing to replace existing %s\n' "$destination" >&2
        exit 1
    fi
else
    ln -s "$app/Contents/MacOS/popsicle" "$destination"
fi
printf 'CLI installed: %s\n' "$destination"
"$destination" --version
