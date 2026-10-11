#!/bin/bash
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
repo_dir=$(CDPATH='' cd -- "$script_dir/.." && pwd -P)
output_app="$repo_dir/dist/Popsicle.app"
deployment_target=${MACOSX_DEPLOYMENT_TARGET:-14.0}

if [[ ! "$deployment_target" =~ ^[0-9]+([.][0-9]+){0,2}$ ]]; then
    printf 'error: invalid MACOSX_DEPLOYMENT_TARGET: %s\n' "$deployment_target" >&2
    exit 1
fi
export MACOSX_DEPLOYMENT_TARGET=$deployment_target

cargo_version=$(awk '
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ {
        gsub(/^[^"]*"|".*$/, "", $0);
        print;
        exit;
    }
' "$repo_dir/gtk/Cargo.toml")
release_version=${RELEASE_VERSION:-$cargo_version}
release_version=${release_version#v}
if [[ ! "$release_version" =~ ^[0-9]+([.][0-9]+){1,2}$ ]]; then
    printf 'error: RELEASE_VERSION must be a numeric version such as 1.3.3: %s\n' \
        "$release_version" >&2
    exit 1
fi

if [ "$(uname -m)" != arm64 ]; then
    printf '%s\n' "error: the macOS app must be built natively on Apple Silicon" >&2
    exit 1
fi
for command_name in \
    awk brew cargo codesign ditto gdk-pixbuf-query-loaders glib-compile-schemas \
    gtk-query-immodules-3.0 lipo perl pkg-config plutil
do
    command -v "$command_name" >/dev/null || {
        printf 'error: missing command: %s\n' "$command_name" >&2
        exit 1
    }
done
for package_name in gtk+-3.0 gdk-pixbuf-2.0 fontconfig gsettings-desktop-schemas; do
    pkg-config --exists "$package_name" || {
        printf 'error: missing pkg-config package: %s\n' "$package_name" >&2
        printf '%s\n' "install macos/Brewfile with: brew bundle --file macos/Brewfile" >&2
        exit 1
    }
done

cd "$repo_dir"
cargo build --locked --release -p popsicle_gtk -p popsicle_cli

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
resources="$contents/Resources"
mkdir -p "$contents/MacOS" "$contents/Frameworks" "$resources"
ditto "$binary" "$contents/MacOS/popsicle-gtk"
ditto "$target_dir/release/popsicle" "$contents/MacOS/popsicle"
ditto "$script_dir/Info.plist" "$contents/Info.plist"

set_plist_string() {
    local key=$1
    local value=$2
    if ! plutil -replace "$key" -string "$value" "$contents/Info.plist" 2>/dev/null; then
        plutil -insert "$key" -string "$value" "$contents/Info.plist"
    fi
}
set_plist_string CFBundleShortVersionString "$release_version"
set_plist_string CFBundleVersion "$release_version"
set_plist_string LSMinimumSystemVersion "$deployment_target"

ditto \
    "$repo_dir/gtk/assets/icons/512x512@2x/apps/com.system76.Popsicle.png" \
    "$resources/Popsicle.png"

gtk_prefix=$(pkg-config --variable=prefix gtk+-3.0)
gtk_libdir=$(pkg-config --variable=libdir gtk+-3.0)
gtk_binary_version=$(pkg-config --variable=gtk_binary_version gtk+-3.0)
pixbuf_module_dir=$(pkg-config --variable=gdk_pixbuf_moduledir gdk-pixbuf-2.0)
pixbuf_binary_version=$(pkg-config --variable=gdk_pixbuf_binary_version gdk-pixbuf-2.0)
gsettings_prefix=$(pkg-config --variable=prefix gsettings-desktop-schemas)
fontconfig_dir=$(pkg-config --variable=confdir fontconfig)
brew_prefix=$(brew --prefix)
adwaita_prefix=$(brew --prefix adwaita-icon-theme)
hicolor_prefix=$(brew --prefix hicolor-icon-theme)

schema_dir="$resources/share/glib-2.0/schemas"
mkdir -p "$schema_dir"
for schema_source in \
    "$gtk_prefix/share/glib-2.0/schemas" \
    "$gsettings_prefix/share/glib-2.0/schemas"
do
    [ -d "$schema_source" ] || continue
    for schema in "$schema_source"/*.xml "$schema_source"/*.gschema.override; do
        [ -f "$schema" ] || continue
        ditto "$schema" "$schema_dir/$(basename "$schema")"
    done
done
PACKAGE_MANAGER_PREFIX="$brew_prefix" /usr/bin/perl -pi -e \
    's{file://\Q$ENV{PACKAGE_MANAGER_PREFIX}\E[^\x27"<]*}{file:///}g' \
    "$schema_dir"/*.xml
glib-compile-schemas --strict "$schema_dir"
if grep -R -Fq "$brew_prefix" "$schema_dir"; then
    printf '%s\n' "error: bundled GSettings schemas contain a Homebrew path" >&2
    exit 1
fi

mkdir -p "$resources/share/icons"
ditto "$hicolor_prefix/share/icons/hicolor" "$resources/share/icons/hicolor"
ditto "$gtk_prefix/share/icons/hicolor" "$resources/share/icons/hicolor"
ditto "$adwaita_prefix/share/icons/Adwaita" "$resources/share/icons/Adwaita"

pixbuf_bundle_dir="$resources/lib/gdk-pixbuf-2.0/$pixbuf_binary_version"
pixbuf_loader_bundle_dir="$pixbuf_bundle_dir/loaders"
mkdir -p "$pixbuf_loader_bundle_dir"
pixbuf_module_count=0
for module in "$pixbuf_module_dir"/*.so; do
    [ -f "$module" ] || continue
    ditto "$module" "$pixbuf_loader_bundle_dir/$(basename "$module")"
    pixbuf_module_count=$((pixbuf_module_count + 1))
done
[ "$pixbuf_module_count" -gt 0 ] || {
    printf 'error: no GDK Pixbuf modules found in %s\n' "$pixbuf_module_dir" >&2
    exit 1
}

pixbuf_cache_raw="$stage_root/gdk-pixbuf-loaders.cache"
gdk-pixbuf-query-loaders "$pixbuf_module_dir"/*.so > "$pixbuf_cache_raw"
CACHE_SOURCE_PREFIX="$pixbuf_module_dir" \
CACHE_BUNDLE_PREFIX="@POPSICLE_RESOURCES@/lib/gdk-pixbuf-2.0/$pixbuf_binary_version/loaders" \
    /usr/bin/perl -pe \
        's/\Q$ENV{CACHE_SOURCE_PREFIX}\E/$ENV{CACHE_BUNDLE_PREFIX}/g' \
        "$pixbuf_cache_raw" > "$pixbuf_bundle_dir/loaders.cache.in"

gtk_im_source_dir="$gtk_libdir/gtk-3.0/$gtk_binary_version/immodules"
gtk_im_bundle_dir="$resources/lib/gtk-3.0/$gtk_binary_version/immodules"
gtk_quartz_module="$gtk_im_source_dir/im-quartz.so"
[ -f "$gtk_quartz_module" ] || {
    printf 'error: missing GTK Quartz input module: %s\n' "$gtk_quartz_module" >&2
    exit 1
}
mkdir -p "$gtk_im_bundle_dir"
ditto "$gtk_quartz_module" "$gtk_im_bundle_dir/im-quartz.so"

gtk_im_cache_raw="$stage_root/gtk-immodules.cache"
gtk-query-immodules-3.0 "$gtk_quartz_module" > "$gtk_im_cache_raw"
CACHE_SOURCE_PREFIX="$gtk_im_source_dir" \
CACHE_BUNDLE_PREFIX="@POPSICLE_RESOURCES@/lib/gtk-3.0/$gtk_binary_version/immodules" \
CACHE_LOCALE_SOURCE="$gtk_prefix/share/locale" \
CACHE_LOCALE_BUNDLE="@POPSICLE_RESOURCES@/share/locale" \
    /usr/bin/perl -pe \
        's/\Q$ENV{CACHE_SOURCE_PREFIX}\E/$ENV{CACHE_BUNDLE_PREFIX}/g;
         s/\Q$ENV{CACHE_LOCALE_SOURCE}\E/$ENV{CACHE_LOCALE_BUNDLE}/g' \
        "$gtk_im_cache_raw" > "$resources/lib/gtk-3.0/$gtk_binary_version/immodules.cache.in"

fontconfig_bundle_dir="$resources/etc/fonts"
mkdir -p "$fontconfig_bundle_dir/conf.d"
PACKAGE_MANAGER_PREFIX="$brew_prefix" /usr/bin/perl -ne '
    print unless m{<cachedir>\Q$ENV{PACKAGE_MANAGER_PREFIX}\E(?:/|<)};
' "$fontconfig_dir/fonts.conf" > "$fontconfig_bundle_dir/fonts.conf"
for config in "$fontconfig_dir/conf.d"/*.conf; do
    [ -f "$config" ] || continue
    ditto "$config" "$fontconfig_bundle_dir/conf.d/$(basename "$config")"
done
if grep -R -Fq "$brew_prefix" "$fontconfig_bundle_dir"; then
    printf '%s\n' "error: bundled fontconfig configuration contains a Homebrew path" >&2
    exit 1
fi
if grep -R -Fq "$brew_prefix" "$resources/lib" --include='*.in'; then
    printf '%s\n' "error: bundled GTK module cache contains a Homebrew path" >&2
    exit 1
fi

"$script_dir/bundle-macho.sh" \
    "$stage_app" "$deployment_target" "$binary" "$pixbuf_module_dir" "$gtk_im_source_dir"

find "$contents/Frameworks" "$resources/lib" -type f \
    \( -name '*.dylib' -o -name '*.so' \) -print0 |
while IFS= read -r -d '' nested_code; do
    codesign --force --sign - --timestamp=none "$nested_code"
done
codesign --force --sign - --timestamp=none "$stage_app"
codesign --verify --deep --strict "$stage_app"

mkdir -p "$repo_dir/dist"
rm -rf "$output_app"
mv "$stage_app" "$output_app"
printf 'Created %s (version %s, macOS %s+, arm64)\n' \
    "$output_app" "$release_version" "$deployment_target"
