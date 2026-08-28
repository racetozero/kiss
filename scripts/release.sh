#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

usage() {
  echo "Usage: scripts/release.sh <check|release> <version> [target]" >&2
  echo "Example: scripts/release.sh check 0.2.0 aarch64-apple-darwin" >&2
  exit 2
}

fail() {
  echo "Release error: $*" >&2
  exit 2
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

mode="${1:-}"
version="${2:-}"
target="${3:-}"

case "$mode" in
  check | release) ;;
  *) usage ;;
esac

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  fail "version must be a Cargo SemVer value without the v prefix"
fi

require_command cargo
require_command dist
require_command git

host_target="$(rustc -vV | sed -n 's/^host: //p')"
[[ -n "$host_target" ]] || fail "rustc did not report a host target"
target="${target:-$host_target}"
tag="v$version"

verify_archive() {
  local archive_name checksum_name binary_path

  case "$target" in
    *-windows-*) archive_name="kiss-$target.zip" ;;
    *) archive_name="kiss-$target.tar.gz" ;;
  esac
  checksum_name="$archive_name.sha256"

  [[ -f "target/distrib/$archive_name" ]] || fail "target/distrib/$archive_name was not built"
  [[ -f "target/distrib/$checksum_name" ]] || fail "target/distrib/$checksum_name was not built"

  if command -v sha256sum >/dev/null 2>&1; then
    (cd target/distrib && sed '/^[[:space:]]*$/d' "$checksum_name" | sha256sum --check -)
  elif command -v shasum >/dev/null 2>&1; then
    (cd target/distrib && sed '/^[[:space:]]*$/d' "$checksum_name" | shasum -a 256 -c -)
  else
    fail "sha256sum or shasum is required to verify the archive"
  fi

  if [[ "$target" == "$host_target" ]]; then
    binary_path="target/$target/dist/kiss"
    [[ "$target" == *-windows-* ]] && binary_path="$binary_path.exe"
    [[ -x "$binary_path" ]] || fail "$binary_path is not executable"
    "$binary_path" --version
  fi

  echo "Release archive is ready: target/distrib/$archive_name"
}

run_checks() {
  echo "Checking release $tag for $target"
  dist plan --tag="$tag"
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo nextest run --workspace --all-targets

  if [[ "$target" == *-apple-darwin ]]; then
    local icf_flags
    icf_flags="-C linker=rust-lld -C linker-flavor=ld64.lld -C link-arg=--icf=safe"
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$icf_flags" \
      dist build --allow-dirty --tag="$tag" --target="$target"
  else
    dist build --allow-dirty --tag="$tag" --target="$target"
  fi

  verify_archive
}

if [[ "$mode" == "check" ]]; then
  run_checks
  exit 0
fi

branch="$(git branch --show-current)"
[[ "$branch" == "main" ]] || fail "release must run from the main branch"
[[ -z "$(git status --porcelain=v1)" ]] || fail "commit or remove all worktree changes first"

if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
  fail "$tag already exists on origin"
else
  remote_status=$?
  [[ "$remote_status" -eq 2 ]] || fail "could not check $tag on origin"
fi

local_tag=false
if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then
  [[ "$(git rev-list -n 1 "$tag")" == "$(git rev-parse HEAD)" ]] || \
    fail "local $tag does not point to HEAD"
  local_tag=true
fi

run_checks
[[ -z "$(git status --porcelain=v1)" ]] || fail "release checks changed the worktree"

if [[ "${KISS_RELEASE_CONFIRM:-}" != "$tag" ]]; then
  [[ -t 0 ]] || fail "set KISS_RELEASE_CONFIRM=$tag for a non-interactive release"
  echo "This will push main and publish $tag."
  read -r -p "Type $tag to continue: " confirmation
  [[ "$confirmation" == "$tag" ]] || fail "release was cancelled"
fi

git push origin main
if [[ "$local_tag" == false ]]; then
  git tag -a "$tag" -m "kiss $tag"
fi
git push origin "$tag"

echo "Published $tag. GitHub Actions will create the release."
