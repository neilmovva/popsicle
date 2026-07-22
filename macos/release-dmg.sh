#!/bin/bash
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
repo_dir=$(CDPATH='' cd -- "$script_dir/.." && pwd -P)

app_path=${POPSICLE_APP_PATH:-"$repo_dir/dist/Popsicle.app"}
release_tag=${RELEASE_TAG:-local}
output_path=${POPSICLE_DMG_PATH:-}
identity=${DEVELOPER_ID_APPLICATION:-}
entitlements=${MACOS_ENTITLEMENTS:-}
signing_keychain=${CODESIGN_KEYCHAIN:-}
dmg_identifier=${DMG_SIGNING_IDENTIFIER:-}
api_key_path=${APP_STORE_CONNECT_API_KEY_PATH:-}
api_key_id=${APP_STORE_CONNECT_KEY_ID:-}
api_issuer_id=${APP_STORE_CONNECT_ISSUER_ID:-}
keychain_profile=${NOTARYTOOL_KEYCHAIN_PROFILE:-}
notary_keychain=${NOTARYTOOL_KEYCHAIN_PATH:-}
notary_timeout=${NOTARYTOOL_TIMEOUT:-30m}
mode=release
force_output=0

usage() {
    cat <<'EOF'
Create a distributable Popsicle disk image from dist/Popsicle.app.

Usage:
  macos/release-dmg.sh [options]

Release mode (the default) signs with Developer ID, notarizes and staples both
the app and the final DMG, and performs Gatekeeper/distribution checks.

Options:
  --app PATH                 Input app (default: dist/Popsicle.app)
  --output PATH              Output DMG
  --identity IDENTITY        Developer ID Application identity
  --entitlements PATH        Entitlements for the outer app signature
  --signing-keychain PATH    Keychain containing the signing identity
  --dmg-identifier ID        Code-signing ID for the DMG
  --api-key PATH             App Store Connect Team API private key (.p8)
  --api-key-id ID            App Store Connect API key ID
  --api-issuer ID            App Store Connect API issuer ID
  --keychain-profile NAME    notarytool keychain profile (instead of API args)
  --notary-keychain PATH     Keychain containing the notarytool profile
  --notary-timeout DURATION  Per-submission wait timeout (default: 30m)
  --adhoc                    Ad-hoc sign and package; do not notarize
  --unsigned                 Remove signatures and package; do not notarize
  --force                    Replace an existing output DMG
  -h, --help                 Show this help

Environment equivalents:
  RELEASE_TAG, POPSICLE_APP_PATH, POPSICLE_DMG_PATH,
  DEVELOPER_ID_APPLICATION, MACOS_ENTITLEMENTS, CODESIGN_KEYCHAIN,
  DMG_SIGNING_IDENTIFIER,
  APP_STORE_CONNECT_API_KEY_PATH, APP_STORE_CONNECT_KEY_ID,
  APP_STORE_CONNECT_ISSUER_ID, NOTARYTOOL_KEYCHAIN_PROFILE,
  NOTARYTOOL_KEYCHAIN_PATH, NOTARYTOOL_TIMEOUT
EOF
}

say() {
    printf '==> %s\n' "$*"
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --app)
            [ "$#" -ge 2 ] || die "--app requires a path"
            app_path=$2
            shift 2
            ;;
        --output)
            [ "$#" -ge 2 ] || die "--output requires a path"
            output_path=$2
            shift 2
            ;;
        --identity)
            [ "$#" -ge 2 ] || die "--identity requires a value"
            identity=$2
            shift 2
            ;;
        --entitlements)
            [ "$#" -ge 2 ] || die "--entitlements requires a path"
            entitlements=$2
            shift 2
            ;;
        --signing-keychain)
            [ "$#" -ge 2 ] || die "--signing-keychain requires a path"
            signing_keychain=$2
            shift 2
            ;;
        --dmg-identifier)
            [ "$#" -ge 2 ] || die "--dmg-identifier requires a value"
            dmg_identifier=$2
            shift 2
            ;;
        --api-key)
            [ "$#" -ge 2 ] || die "--api-key requires a path"
            api_key_path=$2
            shift 2
            ;;
        --api-key-id)
            [ "$#" -ge 2 ] || die "--api-key-id requires a value"
            api_key_id=$2
            shift 2
            ;;
        --api-issuer)
            [ "$#" -ge 2 ] || die "--api-issuer requires a value"
            api_issuer_id=$2
            shift 2
            ;;
        --keychain-profile)
            [ "$#" -ge 2 ] || die "--keychain-profile requires a name"
            keychain_profile=$2
            shift 2
            ;;
        --notary-keychain)
            [ "$#" -ge 2 ] || die "--notary-keychain requires a path"
            notary_keychain=$2
            shift 2
            ;;
        --notary-timeout)
            [ "$#" -ge 2 ] || die "--notary-timeout requires a duration"
            notary_timeout=$2
            shift 2
            ;;
        --adhoc)
            mode=adhoc
            shift
            ;;
        --unsigned)
            mode=unsigned
            shift
            ;;
        --force)
            force_output=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            [ "$#" -eq 0 ] || die "unexpected positional argument: $1"
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

[ -d "$app_path" ] || die "input app does not exist: $app_path"
[ -f "$app_path/Contents/Info.plist" ] || die "input is not a macOS app bundle: $app_path"

case "$release_tag" in
    ''|*[!A-Za-z0-9._-]*)
        die "RELEASE_TAG may contain only letters, digits, period, underscore, and hyphen"
        ;;
esac

if [ -z "$output_path" ]; then
    output_path="$repo_dir/dist/Popsicle-${release_tag}-arm64.dmg"
fi
case "$output_path" in
    *.dmg) ;;
    *) die "output path must end in .dmg: $output_path" ;;
esac

output_dir=$(dirname -- "$output_path")
mkdir -p "$output_dir"
output_dir=$(CDPATH='' cd -- "$output_dir" && pwd -P)
output_path="$output_dir/$(basename -- "$output_path")"

if [ -e "$output_path" ] && [ "$force_output" -ne 1 ]; then
    die "output already exists (pass --force to replace it): $output_path"
fi

for command_name in codesign ditto du file find hdiutil lipo otool plutil readlink spctl xattr xcrun; do
    require_command "$command_name"
done
if [ "$mode" = release ]; then
    require_command security
fi

executable_name=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' \
    "$app_path/Contents/Info.plist" 2>/dev/null) || die "CFBundleExecutable is missing"
bundle_identifier=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \
    "$app_path/Contents/Info.plist" 2>/dev/null) || die "CFBundleIdentifier is missing"
if [ -z "$dmg_identifier" ]; then
    dmg_identifier="$bundle_identifier.dmg"
fi
main_executable="$app_path/Contents/MacOS/$executable_name"
[ -f "$main_executable" ] || die "bundle executable is missing: $main_executable"

main_archs=$(lipo -archs "$main_executable")
case " $main_archs " in
    *' arm64 '*) ;;
    *) die "bundle executable does not contain arm64 code: $main_archs" ;;
esac

if [ -n "$entitlements" ]; then
    [ -f "$entitlements" ] || die "entitlements file does not exist: $entitlements"
fi

if [ -n "$signing_keychain" ]; then
    [ -f "$signing_keychain" ] || die "signing keychain does not exist: $signing_keychain"
fi

notary_auth=()
if [ "$mode" = release ]; then
    [ -n "$identity" ] || die "set DEVELOPER_ID_APPLICATION or pass --identity"

    identity_report=$(security find-identity -v -p codesigning "$signing_keychain" 2>/dev/null || true)
    if [ -z "$signing_keychain" ]; then
        identity_report=$(security find-identity -v -p codesigning 2>/dev/null || true)
    fi
    identity_line=$(printf '%s\n' "$identity_report" | awk -v wanted="$identity" \
        'index($0, wanted) && index($0, "Developer ID Application:") { print; exit }')
    [ -n "$identity_line" ] || die "identity is not an available Developer ID Application certificate: $identity"

    if [ -n "$keychain_profile" ]; then
        if [ -n "$api_key_path$api_key_id$api_issuer_id" ]; then
            die "use either a notarytool keychain profile or API-key arguments, not both"
        fi
        notary_auth=(--keychain-profile "$keychain_profile")
        if [ -n "$notary_keychain" ]; then
            [ -f "$notary_keychain" ] || die "notary keychain does not exist: $notary_keychain"
            notary_auth+=(--keychain "$notary_keychain")
        fi
    else
        [ -n "$api_key_path" ] || die "set APP_STORE_CONNECT_API_KEY_PATH or pass --api-key"
        [ -f "$api_key_path" ] || die "App Store Connect API key does not exist: $api_key_path"
        [ -n "$api_key_id" ] || die "set APP_STORE_CONNECT_KEY_ID or pass --api-key-id"
        [ -n "$api_issuer_id" ] || die "set APP_STORE_CONNECT_ISSUER_ID or pass --api-issuer"
        [ -z "$notary_keychain" ] || die "--notary-keychain requires --keychain-profile"
        notary_auth=(--key "$api_key_path" --key-id "$api_key_id" --issuer "$api_issuer_id")
    fi
fi

if [ -e "$output_path" ]; then
    rm -f -- "$output_path"
fi

temp_root=$(mktemp -d "$output_dir/.popsicle-release.XXXXXX")
mounted_device=

cleanup() {
    status=$?
    if [ -n "$mounted_device" ]; then
        hdiutil detach "$mounted_device" >/dev/null 2>&1 || true
    fi
    if [ -n "${temp_root:-}" ] && [ -d "$temp_root" ]; then
        rm -rf -- "$temp_root"
    fi
    return "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

stage_app="$temp_root/Popsicle.app"
say "Staging $app_path"
ditto "$app_path" "$stage_app"
chmod -R u+w "$stage_app"
xattr -cr "$stage_app"

dependency_failures=0
check_macho_dependencies() {
    target=$1
    first_line=1
    while IFS= read -r dependency_line; do
        if [ "$first_line" -eq 1 ]; then
            first_line=0
            continue
        fi
        dependency_line=${dependency_line#"${dependency_line%%[![:space:]]*}"}
        dependency=${dependency_line%% *}
        case "$dependency" in
            /System/Library/*|/usr/lib/*|@rpath/*|@loader_path/*|@executable_path/*)
                ;;
            /*)
                warn "$target has a non-system absolute dependency: $dependency"
                dependency_failures=$((dependency_failures + 1))
                ;;
        esac
    done < <(otool -L "$target")
}

while IFS= read -r -d '' candidate; do
    file_type=$(file -b "$candidate")
    case "$file_type" in
        *Mach-O*) check_macho_dependencies "$candidate" ;;
    esac
done < <(find "$stage_app/Contents" -type f -print0)

if [ "$dependency_failures" -gt 0 ]; then
    if [ "$mode" = release ]; then
        die "app is not self-contained ($dependency_failures absolute non-system dependencies)"
    fi
    warn "continuing in $mode test mode with a non-self-contained app"
fi

remove_signature_if_present() {
    target=$1
    if codesign --display --verbose=0 "$target" >/dev/null 2>&1; then
        codesign --remove-signature "$target"
    fi
}

remove_all_signatures() {
    while IFS= read -r -d '' candidate; do
        file_type=$(file -b "$candidate")
        case "$file_type" in
            *Mach-O*) remove_signature_if_present "$candidate" ;;
        esac
    done < <(find "$stage_app/Contents" -type f -print0)

    while IFS= read -r -d '' bundle; do
        remove_signature_if_present "$bundle"
    done < <(find "$stage_app/Contents" -depth -type d \
        \( -name '*.app' -o -name '*.appex' -o -name '*.bundle' \
        -o -name '*.framework' -o -name '*.plugin' -o -name '*.xpc' \) -print0)
    remove_signature_if_present "$stage_app"
}

sign_target() {
    target=$1
    if [ "$mode" = release ]; then
        developer_codesign --options runtime --timestamp "$target"
    else
        codesign --force --sign - --timestamp=none "$target"
    fi
}

developer_codesign() {
    if [ -n "$signing_keychain" ]; then
        codesign --force --sign "$identity" --keychain "$signing_keychain" "$@"
    else
        codesign --force --sign "$identity" "$@"
    fi
}

sign_app_inside_out() {
    while IFS= read -r -d '' candidate; do
        file_type=$(file -b "$candidate")
        case "$file_type" in
            *Mach-O*) sign_target "$candidate" ;;
        esac
    done < <(find "$stage_app/Contents" -type f -print0)

    while IFS= read -r -d '' bundle; do
        sign_target "$bundle"
    done < <(find "$stage_app/Contents" -depth -type d \
        \( -name '*.app' -o -name '*.appex' -o -name '*.bundle' \
        -o -name '*.framework' -o -name '*.plugin' -o -name '*.xpc' \) -print0)

    if [ "$mode" = release ] && [ -n "$entitlements" ]; then
        developer_codesign --options runtime --timestamp \
            --entitlements "$entitlements" "$stage_app"
    else
        sign_target "$stage_app"
    fi
}

if [ "$mode" = unsigned ]; then
    say "Removing code signatures for an unsigned test package"
    remove_all_signatures
else
    if [ "$mode" = release ]; then
        say "Signing nested code with $identity"
    else
        say "Applying ad-hoc signatures for a local test package"
    fi
    sign_app_inside_out
    codesign --verify --deep --strict --all-architectures --verbose=4 "$stage_app"
    codesign --display --verbose=4 "$stage_app"
fi

json_field() {
    field=$1
    json_path=$2
    plutil -extract "$field" raw -o - "$json_path" 2>/dev/null || true
}

notarize_artifact() {
    artifact=$1
    label=$2
    result_path="$temp_root/${label}-notary-result.json"
    stderr_path="$temp_root/${label}-notary-stderr.txt"
    log_path="$temp_root/${label}-notary-log.json"

    say "Submitting $label for notarization"
    set +e
    xcrun notarytool submit "$artifact" "${notary_auth[@]}" --wait \
        --timeout "$notary_timeout" --output-format json \
        >"$result_path" 2>"$stderr_path"
    submit_exit=$?
    set -e

    if [ -s "$stderr_path" ]; then
        cat "$stderr_path" >&2
    fi
    if [ -s "$result_path" ]; then
        cat "$result_path"
    fi

    submission_id=$(json_field id "$result_path")
    submission_status=$(json_field status "$result_path")
    if [ -n "$submission_id" ]; then
        say "Retrieving notarization log for $label ($submission_id)"
        set +e
        xcrun notarytool log "${notary_auth[@]}" "$submission_id" "$log_path"
        log_exit=$?
        set -e
        if [ -s "$log_path" ]; then
            cat "$log_path"
        fi
        [ "$log_exit" -eq 0 ] || die "could not retrieve the $label notarization log"
    fi

    [ "$submit_exit" -eq 0 ] || die "$label notarization submission failed"
    [ -n "$submission_id" ] || die "notarytool did not return a submission ID for $label"
    [ "$submission_status" = Accepted ] || die "$label notarization status is: ${submission_status:-unknown}"
}

staple_with_retry() {
    target=$1
    description=$2
    attempt=1
    while [ "$attempt" -le 3 ]; do
        if xcrun stapler staple -v "$target"; then
            xcrun stapler validate -v "$target"
            return 0
        fi
        if [ "$attempt" -lt 3 ]; then
            warn "$description ticket is not available yet; retrying"
            sleep_seconds=$((attempt * 5))
            sleep "$sleep_seconds"
        fi
        attempt=$((attempt + 1))
    done
    die "could not staple $description after three attempts"
}

if [ "$mode" = release ]; then
    if command -v syspolicy_check >/dev/null 2>&1; then
        syspolicy_check notary-submission "$stage_app" --verbose
    else
        warn "syspolicy_check is unavailable; relying on codesign and notarytool"
    fi

    app_archive="$temp_root/Popsicle.zip"
    say "Creating the app notarization archive"
    ditto -c -k --keepParent "$stage_app" "$app_archive"
    notarize_artifact "$app_archive" application
    staple_with_retry "$stage_app" application
fi

volume_name="Popsicle $release_tag"
temporary_dmg="$temp_root/$(basename -- "$output_path")"
writable_dmg="$temp_root/Popsicle-writable.dmg"
app_size_kb=$(du -sk "$stage_app" | awk '{ print $1 }')
dmg_size_kb=$((app_size_kb + 65536))

say "Creating the disk image filesystem"
hdiutil create -quiet -ov -size "${dmg_size_kb}k" -fs APFS \
    -volname "$volume_name" -type UDIF "$writable_dmg"

dmg_build_mount="$temp_root/dmg-build-mount"
mkdir -p "$dmg_build_mount"
build_attach_output="$temp_root/hdiutil-build-attach.txt"
hdiutil attach -readwrite -nobrowse -noautoopen -mountpoint "$dmg_build_mount" \
    "$writable_dmg" >"$build_attach_output"
mounted_device=$(awk '/^\/dev\// { print $1; exit }' "$build_attach_output")
[ -n "$mounted_device" ] || die "could not determine the writable disk device"

ditto "$stage_app" "$dmg_build_mount/Popsicle.app"
ln -s /Applications "$dmg_build_mount/Applications"
touch "$dmg_build_mount/.metadata_never_index"
sync
hdiutil detach "$mounted_device"
mounted_device=

say "Compressing the read-only disk image"
hdiutil convert -quiet "$writable_dmg" -format UDZO -imagekey zlib-level=9 \
    -o "$temporary_dmg"

if [ "$mode" = release ]; then
    say "Signing the disk image with $identity"
    developer_codesign --identifier "$dmg_identifier" --timestamp "$temporary_dmg"
elif [ "$mode" = adhoc ]; then
    say "Applying an ad-hoc signature to the disk image"
    codesign --force --sign - --identifier "$dmg_identifier" --timestamp=none \
        "$temporary_dmg"
fi

if [ "$mode" = release ]; then
    notarize_artifact "$temporary_dmg" disk-image
    staple_with_retry "$temporary_dmg" disk-image
fi

say "Verifying the disk image"
hdiutil verify "$temporary_dmg"
image_format=$(hdiutil imageinfo "$temporary_dmg" | awk -F: \
    '$1 ~ /^[[:space:]]*Format[[:space:]]*$/ { gsub(/[[:space:]]/, "", $2); print $2; exit }')
[ "$image_format" = UDZO ] || die "unexpected disk image format: ${image_format:-unknown}"

if [ "$mode" != unsigned ]; then
    codesign --verify --verbose=4 "$temporary_dmg"
fi
if [ "$mode" = release ]; then
    xcrun stapler validate -v "$temporary_dmg"
    spctl --assess --type open --context context:primary-signature \
        --verbose=4 "$temporary_dmg"
fi

verify_mount="$temp_root/verify-mount"
mkdir -p "$verify_mount"
attach_output_path="$temp_root/hdiutil-attach.txt"
hdiutil attach -readonly -nobrowse -noautoopen -verify -mountpoint "$verify_mount" \
    "$temporary_dmg" >"$attach_output_path"
cat "$attach_output_path"
mounted_device=$(awk '/^\/dev\// { print $1; exit }' "$attach_output_path")
[ -n "$mounted_device" ] || die "could not determine the mounted disk device"

mounted_app="$verify_mount/Popsicle.app"
[ -d "$mounted_app" ] || die "disk image does not contain Popsicle.app"
[ -L "$verify_mount/Applications" ] || die "disk image does not contain an Applications symlink"
[ "$(readlink "$verify_mount/Applications")" = /Applications ] || \
    die "Applications symlink has the wrong target"

if [ "$mode" = unsigned ]; then
    if codesign --verify "$mounted_app" >/dev/null 2>&1; then
        die "unsigned package unexpectedly contains a signed app"
    fi
else
    codesign --verify --deep --strict --all-architectures --verbose=4 "$mounted_app"
fi

if [ "$mode" = release ]; then
    xcrun stapler validate -v "$mounted_app"
    spctl --assess --type execute --verbose=4 "$mounted_app"
    if command -v syspolicy_check >/dev/null 2>&1; then
        syspolicy_check distribution "$mounted_app" --verbose
    else
        warn "syspolicy_check is unavailable; relying on codesign and spctl"
    fi
fi

hdiutil detach "$mounted_device"
mounted_device=

mv "$temporary_dmg" "$output_path"
say "Created $output_path"
