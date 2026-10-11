#!/bin/bash
set -euo pipefail

if [ "$#" -ne 5 ]; then
    printf '%s\n' \
        "usage: bundle-macho.sh APP_PATH DEPLOYMENT_TARGET MAIN_BINARY_SOURCE PIXBUF_MODULE_SOURCE GTK_IM_MODULE_SOURCE" >&2
    exit 2
fi

app_path=$1
deployment_target=$2
main_binary_source=$3
pixbuf_module_source=$4
gtk_im_module_source=$5
contents="$app_path/Contents"
frameworks="$contents/Frameworks"
main_binary="$contents/MacOS/popsicle-gtk"

for command_name in codesign otool install_name_tool lipo ditto xcrun perl; do
    command -v "$command_name" >/dev/null || {
        printf 'error: missing command: %s\n' "$command_name" >&2
        exit 1
    }
done

is_system_dependency() {
    case "$1" in
        /System/Library/*|/usr/lib/*|/Library/Apple/System/Library/*) return 0 ;;
        *) return 1 ;;
    esac
}

canonical_path() {
    /usr/bin/perl -MCwd=abs_path -e '
        my $path = abs_path($ARGV[0]);
        die "unable to resolve $ARGV[0]\n" unless defined $path;
        print $path;
    ' "$1"
}

macho_dependencies() {
    otool -L "$1" | sed -n '2,$ {
        s/^[[:space:]]*//
        s/[[:space:]]*(compatibility version.*$//
        p
    }'
}

macho_rpaths() {
    otool -l "$1" | awk '
        $1 == "cmd" && $2 == "LC_RPATH" { want_path = 1; next }
        want_path && $1 == "path" { print $2; want_path = 0 }
    '
}

macho_install_id() {
    otool -D "$1" 2>/dev/null | sed -n '2p'
}

library_sources=()
library_names=()
scan_sources=("$(canonical_path "$main_binary_source")")
scan_destinations=("$main_binary")
scan_kinds=(main)
scan_sources+=("$(canonical_path "$(dirname "$main_binary_source")/popsicle")")
scan_destinations+=("$contents/MacOS/popsicle")
scan_kinds+=(main)

for module in "$contents"/PlugIns/libpixbufloader*.so; do
    [ -f "$module" ] || continue
    scan_sources+=("$(canonical_path "$pixbuf_module_source/$(basename "$module")")")
    scan_destinations+=("$module")
    scan_kinds+=(module)
done
for module in "$contents"/PlugIns/im-*.so; do
    [ -f "$module" ] || continue
    scan_sources+=("$(canonical_path "$gtk_im_module_source/$(basename "$module")")")
    scan_destinations+=("$module")
    scan_kinds+=(module)
done

registered_name=
find_registered_library() {
    local source=$1
    local index
    registered_name=
    for ((index = 0; index < ${#library_sources[@]}; index++)); do
        if [ "${library_sources[$index]}" = "$source" ]; then
            registered_name=${library_names[$index]}
            return 0
        fi
    done
    return 1
}

register_library() {
    local source
    local name
    local index

    source=$(canonical_path "$1")
    if find_registered_library "$source"; then
        return 0
    fi

    name=$(basename "$source")
    for ((index = 0; index < ${#library_names[@]}; index++)); do
        if [ "${library_names[$index]}" = "$name" ]; then
            printf 'error: two libraries have the same bundle name: %s and %s\n' \
                "${library_sources[$index]}" "$source" >&2
            exit 1
        fi
    done

    ditto "$source" "$frameworks/$name"
    library_sources+=("$source")
    library_names+=("$name")
    scan_sources+=("$source")
    scan_destinations+=("$frameworks/$name")
    scan_kinds+=(framework)
    registered_name=$name
}

resolved_dependency=
resolve_dependency() {
    local dependency=$1
    local source=$2
    local candidate
    local suffix
    local rpath

    resolved_dependency=
    case "$dependency" in
        /*)
            candidate=$dependency
            ;;
        @loader_path/*)
            candidate="$(dirname "$source")/${dependency#@loader_path/}"
            ;;
        @executable_path/*)
            candidate="$(dirname "$main_binary_source")/${dependency#@executable_path/}"
            ;;
        @rpath/*)
            suffix=${dependency#@rpath/}
            while IFS= read -r rpath; do
                [ -n "$rpath" ] || continue
                case "$rpath" in
                    @loader_path/*)
                        candidate="$(dirname "$source")/${rpath#@loader_path/}/$suffix"
                        ;;
                    @executable_path/*)
                        candidate="$(dirname "$main_binary_source")/${rpath#@executable_path/}/$suffix"
                        ;;
                    /*)
                        candidate="$rpath/$suffix"
                        ;;
                    *)
                        continue
                        ;;
                esac
                if [ -e "$candidate" ]; then
                    resolved_dependency=$(canonical_path "$candidate")
                    return 0
                fi
            done < <(macho_rpaths "$source")

            for ((candidate_index = 0; candidate_index < ${#library_names[@]}; candidate_index++)); do
                if [ "${library_names[$candidate_index]}" = "$(basename "$suffix")" ]; then
                    resolved_dependency=${library_sources[$candidate_index]}
                    return 0
                fi
            done
            return 1
            ;;
        *)
            return 1
            ;;
    esac

    [ -e "$candidate" ] || return 1
    resolved_dependency=$(canonical_path "$candidate")
}

mkdir -p "$frameworks"

scan_index=0
while [ "$scan_index" -lt "${#scan_sources[@]}" ]; do
    source=${scan_sources[$scan_index]}
    destination=${scan_destinations[$scan_index]}
    install_id=$(macho_install_id "$source")

    while IFS= read -r dependency; do
        [ -n "$dependency" ] || continue
        [ "$dependency" = "$install_id" ] && continue
        if is_system_dependency "$dependency"; then
            continue
        fi
        if ! resolve_dependency "$dependency" "$source"; then
            printf 'error: cannot resolve dependency %s from %s\n' "$dependency" "$source" >&2
            exit 1
        fi
        register_library "$resolved_dependency"
    done < <(macho_dependencies "$destination")

    scan_index=$((scan_index + 1))
done

reference_for_kind=
make_bundle_reference() {
    local kind=$1
    local name=$2
    case "$kind" in
        main) reference_for_kind="@rpath/$name" ;;
        framework) reference_for_kind="@loader_path/$name" ;;
        module) reference_for_kind="@loader_path/../Frameworks/$name" ;;
        *) printf 'error: unknown Mach-O kind: %s\n' "$kind" >&2; exit 1 ;;
    esac
}

for ((scan_index = 0; scan_index < ${#scan_sources[@]}; scan_index++)); do
    source=${scan_sources[$scan_index]}
    destination=${scan_destinations[$scan_index]}
    kind=${scan_kinds[$scan_index]}
    install_id=$(macho_install_id "$source")

    while IFS= read -r dependency; do
        [ -n "$dependency" ] || continue
        [ "$dependency" = "$install_id" ] && continue
        if is_system_dependency "$dependency"; then
            continue
        fi
        if ! resolve_dependency "$dependency" "$source"; then
            printf 'error: cannot resolve dependency %s from %s\n' "$dependency" "$source" >&2
            exit 1
        fi
        if ! find_registered_library "$resolved_dependency"; then
            printf 'error: dependency was not bundled: %s\n' "$resolved_dependency" >&2
            exit 1
        fi
        make_bundle_reference "$kind" "$registered_name"
        install_name_tool -change "$dependency" "$reference_for_kind" "$destination"
    done < <(macho_dependencies "$destination")

    if [ -n "$install_id" ]; then
        case "$kind" in
            framework) new_install_id="@rpath/$(basename "$destination")" ;;
            module) new_install_id="@loader_path/$(basename "$destination")" ;;
            *) printf 'error: unexpected install ID in %s\n' "$destination" >&2; exit 1 ;;
        esac
        install_name_tool -id "$new_install_id" "$destination"
    fi

    while IFS= read -r rpath; do
        [ -n "$rpath" ] || continue
        case "$rpath" in
            @loader_path/*|@executable_path/*|/System/Library/*|/usr/lib/*|/Library/Apple/System/Library/*)
                ;;
            *)
                install_name_tool -delete_rpath "$rpath" "$destination"
                ;;
        esac
    done < <(macho_rpaths "$destination")
done

if ! macho_rpaths "$main_binary" | grep -Fxq '@executable_path/../Frameworks'; then
    install_name_tool -add_rpath '@executable_path/../Frameworks' "$main_binary"
fi

version_is_at_most() {
    /usr/bin/awk -v actual="$1" -v maximum="$2" 'BEGIN {
        split(actual, a, ".");
        split(maximum, b, ".");
        for (i = 1; i <= 3; i++) {
            av = (a[i] == "" ? 0 : a[i]) + 0;
            bv = (b[i] == "" ? 0 : b[i]) + 0;
            if (av < bv) exit 0;
            if (av > bv) exit 1;
        }
        exit 0;
    }'
}

verify_reference() {
    local dependency=$1
    local image=$2
    local target

    if is_system_dependency "$dependency"; then
        return 0
    fi
    case "$dependency" in
        @rpath/*)
            target="$frameworks/${dependency#@rpath/}"
            ;;
        @loader_path/*)
            target="$(dirname "$image")/${dependency#@loader_path/}"
            ;;
        @executable_path/*)
            target="$contents/MacOS/${dependency#@executable_path/}"
            ;;
        *)
            printf 'error: non-portable dependency in %s: %s\n' "$image" "$dependency" >&2
            return 1
            ;;
    esac
    if [ ! -e "$target" ]; then
        printf 'error: bundled dependency does not exist for %s: %s\n' "$image" "$dependency" >&2
        return 1
    fi
}

for ((scan_index = 0; scan_index < ${#scan_destinations[@]}; scan_index++)); do
    image=${scan_destinations[$scan_index]}
    case " $(lipo -archs "$image") " in
        *" arm64 "*) ;;
        *) printf 'error: bundled Mach-O is not arm64: %s\n' "$image" >&2; exit 1 ;;
    esac

    minos=$(xcrun vtool -show-build "$image" | awk '$1 == "minos" { print $2; exit }')
    if [ -z "$minos" ]; then
        printf 'error: unable to read minimum macOS version from %s\n' "$image" >&2
        exit 1
    fi
    if ! version_is_at_most "$minos" "$deployment_target"; then
        printf 'error: %s requires macOS %s, newer than requested deployment target %s\n' \
            "$image" "$minos" "$deployment_target" >&2
        exit 1
    fi

    while IFS= read -r rpath; do
        [ -n "$rpath" ] || continue
        case "$rpath" in
            @loader_path/*|@executable_path/*|/System/Library/*|/usr/lib/*|/Library/Apple/System/Library/*) ;;
            *)
                printf 'error: non-portable runtime search path in %s: %s\n' \
                    "$image" "$rpath" >&2
                exit 1
                ;;
        esac
    done < <(macho_rpaths "$image")

    while IFS= read -r dependency; do
        [ -n "$dependency" ] || continue
        verify_reference "$dependency" "$image"
    done < <(macho_dependencies "$image")
done

printf 'Bundled and verified %d Mach-O images (%d libraries)\n' \
    "${#scan_destinations[@]}" "${#library_sources[@]}"
