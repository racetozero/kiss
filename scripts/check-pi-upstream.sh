#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$repo_root/Cargo.toml"

metadata_value() {
  local key="$1"
  awk -v key="$key" '
    $0 == "[workspace.metadata.pi]" { in_pi = 1; next }
    in_pi && /^\[/ { exit }
    in_pi {
      split($0, pair, "=")
      name = pair[1]
      gsub(/[[:space:]]/, "", name)
      if (name == key) {
        value = substr($0, index($0, "=") + 1)
        sub(/^[[:space:]]*"/, "", value)
        sub(/"[[:space:]]*$/, "", value)
        print value
        exit
      }
    }
  ' "$manifest"
}

repository="$(metadata_value repository)"
baseline_release="$(metadata_value release)"
release_commit="$(metadata_value release_commit)"
parity_commit="$(metadata_value parity_commit)"

if [[ -z "$repository" || -z "$baseline_release" || -z "$release_commit" || -z "$parity_commit" ]]; then
  echo "Pi baseline metadata is incomplete in $manifest" >&2
  exit 2
fi

if [[ -n "${PI_LATEST_RELEASE:-}" ]]; then
  latest_release="$PI_LATEST_RELEASE"
else
  if ! command -v gh >/dev/null 2>&1; then
    echo "gh is required to check the latest Pi release" >&2
    exit 2
  fi
  latest_release="$(gh api "repos/$repository/releases/latest" --jq .tag_name)"
fi

if [[ "$latest_release" == "$baseline_release" ]]; then
  echo "Pi baseline is current: $baseline_release ($release_commit)"
  echo "Parity was audited through: $parity_commit"
  exit 0
fi

echo "A newer Pi release is available."
echo "Kiss baseline: $baseline_release"
echo "Latest Pi release: $latest_release"
echo "Review: https://github.com/$repository/compare/$parity_commit...$latest_release"
exit 1
