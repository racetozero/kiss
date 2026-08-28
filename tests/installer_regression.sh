#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_root="$(mktemp -d "${TMPDIR:-/tmp}/kiss-installer-test.XXXXXX")"
trap 'rm -rf "$temporary_root"' EXIT HUP INT TERM

target="x86_64-unknown-linux-gnu"
tag="v0.0.1"
archive_name="kiss-$target.tar.gz"
release_directory="$temporary_root/releases/download/$tag"
payload_directory="$temporary_root/payload/kiss-$target"
install_directory="$temporary_root/install"
mkdir -p "$release_directory" "$payload_directory" "$install_directory"

write_archive() {
    local payload="$1"
    printf '%s\n' "$payload" >"$payload_directory/kiss"
    chmod +x "$payload_directory/kiss"
    tar -czf "$release_directory/$archive_name" -C "$temporary_root/payload" "kiss-$target"
    if command -v sha256sum >/dev/null 2>&1; then
        hash="$(sha256sum "$release_directory/$archive_name" | awk '{ print $1 }')"
    else
        hash="$(shasum -a 256 "$release_directory/$archive_name" | awk '{ print $1 }')"
    fi
    printf '%s  %s\n' "$hash" "$archive_name" >"$release_directory/$archive_name.sha256"
}

run_installer() {
    KISS_VERSION="0.0.1" \
        KISS_TARGET="$target" \
        KISS_INSTALL_DIR="$install_directory" \
        KISS_RELEASES_URL="file://$temporary_root/releases" \
        sh "$repo_root/install.sh"
}

write_archive "first"
run_installer
[[ "$(cat "$install_directory/kiss")" == "first" ]]
[[ -x "$install_directory/kiss" ]]
echo "ok: shell installer installs a verified archive"

write_archive "replacement"
run_installer
[[ "$(cat "$install_directory/kiss")" == "replacement" ]]
echo "ok: shell installer replaces an existing binary"

printf '%064d  %s\n' 0 "$archive_name" >"$release_directory/$archive_name.sha256"
if run_installer >/dev/null 2>&1; then
    echo "checksum failure unexpectedly succeeded" >&2
    exit 1
fi
[[ "$(cat "$install_directory/kiss")" == "replacement" ]]
echo "ok: checksum failure keeps the existing binary"

if KISS_VERSION="0.0.1" \
    KISS_TARGET="unsupported-target" \
    KISS_INSTALL_DIR="$install_directory" \
    KISS_RELEASES_URL="file://$temporary_root/releases" \
    sh "$repo_root/install.sh" >/dev/null 2>&1; then
    echo "unsupported target unexpectedly succeeded" >&2
    exit 1
fi
echo "ok: unsupported shell target fails"

fake_bin="$temporary_root/fake-bin"
mkdir "$fake_bin"
cat >"$fake_bin/gh" <<'EOF'
#!/bin/sh
set -eu
case "${1:-} ${2:-}" in
    "auth status") exit 0 ;;
    "release view") printf '%s\n' "v0.0.1" ;;
    "release download")
        destination=''
        while [ "$#" -gt 0 ]; do
            if [ "$1" = "--dir" ]; then
                shift
                destination="$1"
            fi
            shift
        done
        [ -n "$destination" ]
        cp "$FAKE_RELEASE_DIRECTORY/$FAKE_ARCHIVE_NAME" "$destination/$FAKE_ARCHIVE_NAME"
        cp "$FAKE_RELEASE_DIRECTORY/$FAKE_ARCHIVE_NAME.sha256" "$destination/$FAKE_ARCHIVE_NAME.sha256"
        ;;
    *) exit 2 ;;
esac
EOF
chmod +x "$fake_bin/gh"
write_archive "private-release"
FAKE_RELEASE_DIRECTORY="$release_directory" \
    FAKE_ARCHIVE_NAME="$archive_name" \
    PATH="$fake_bin:$PATH" \
    KISS_VERSION="latest" \
    KISS_TARGET="$target" \
    KISS_INSTALL_DIR="$install_directory" \
    sh "$repo_root/install.sh"
[[ "$(cat "$install_directory/kiss")" == "private-release" ]]
echo "ok: authenticated gh path installs a private release"
