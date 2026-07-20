#!/bin/bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd -P)
output_app="$repo_dir/dist/Popsicle.app"

if [ "$(uname -m)" != arm64 ]; then
    printf '%s\n' "error: the macOS app must be built natively on Apple Silicon" >&2
    exit 1
fi
for command_name in cargo pkg-config; do
    command -v "$command_name" >/dev/null || {
        printf 'error: missing command: %s\n' "$command_name" >&2
        exit 1
    }
done
pkg-config --exists gtk+-3.0 || {
    printf '%s\n' "error: install macos/Brewfile with: brew bundle --file macos/Brewfile" >&2
    exit 1
}

cd "$repo_dir"
cargo build --locked --release -p popsicle_gtk

target_dir=${CARGO_TARGET_DIR:-$repo_dir/target}
case "$target_dir" in
    /*) ;;
    *) target_dir="$repo_dir/$target_dir" ;;
esac
binary="$target_dir/release/popsicle-gtk"
[ -x "$binary" ] || {
    printf 'error: missing release binary: %s\n' "$binary" >&2
    exit 1
}
case " $(lipo -archs "$binary") " in
    *" arm64 "*) ;;
    *) printf '%s\n' "error: popsicle-gtk is not arm64" >&2; exit 1 ;;
esac

stage_root=$(mktemp -d "${TMPDIR:-/tmp}/popsicle-app.XXXXXX")
trap 'rm -rf "$stage_root"' EXIT HUP INT TERM
stage_app="$stage_root/Popsicle.app"
contents="$stage_app/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources"
ditto "$binary" "$contents/MacOS/popsicle-gtk"
ditto "$script_dir/Info.plist" "$contents/Info.plist"

ditto \
    "$repo_dir/gtk/assets/icons/512x512@2x/apps/com.system76.Popsicle.png" \
    "$contents/Resources/Popsicle.png"

codesign --force --sign - --timestamp=none "$stage_app"
codesign --verify --deep --strict "$stage_app"

mkdir -p "$repo_dir/dist"
rm -rf "$output_app"
mv "$stage_app" "$output_app"
printf 'Created %s\n' "$output_app"
