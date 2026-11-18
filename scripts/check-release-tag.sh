#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

package_version="$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' Cargo.toml | head -n 1)"
if [[ ! "$package_version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  echo "Cargo.toml must declare a three-component package version; found: ${package_version:-<missing>}" >&2
  exit 1
fi
package_major="${BASH_REMATCH[1]}"
package_minor="${BASH_REMATCH[2]}"
expected_tag="v${package_major}.${package_minor}"

validate_tag() {
  local tag="$1"
  if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "Invalid release tag '$tag': use exactly vMAJOR.MINOR (for example v0.1)." >&2
    return 1
  fi
  local tag_major="${BASH_REMATCH[1]}"
  local tag_minor="${BASH_REMATCH[2]}"
  if [[ "$tag_major.$tag_minor" != "$package_major.$package_minor" ]]; then
    echo "Release tag '$tag' does not match Cargo package version $package_version (expected $expected_tag)." >&2
    return 1
  fi
}

if [[ "${1:-}" == "--self-test" ]]; then
  if [[ "$#" -ne 1 ]]; then
    echo "--self-test does not accept additional arguments" >&2
    exit 2
  fi
  validate_tag "$expected_tag"
  if validate_tag "${expected_tag}.0" >/dev/null 2>&1; then
    echo "Self-test failed: accepted a three-component tag" >&2
    exit 1
  fi
  if validate_tag "v${package_major}.$((package_minor + 1))" >/dev/null 2>&1; then
    echo "Self-test failed: accepted a tag for a different package version" >&2
    exit 1
  fi
  if validate_tag "v$((package_major + 1)).${package_minor}" >/dev/null 2>&1; then
    echo "Self-test failed: accepted a tag for a different major version" >&2
    exit 1
  fi
  if validate_tag "v0${package_major}.${package_minor}" >/dev/null 2>&1; then
    echo "Self-test failed: accepted a leading-zero version component" >&2
    exit 1
  fi
  if validate_tag "${expected_tag}-rc.1" >/dev/null 2>&1; then
    echo "Self-test failed: accepted a prerelease tag" >&2
    exit 1
  fi
  echo "Release tag policy tests passed for $expected_tag."
  exit 0
fi

if [[ "$#" -ne 1 ]]; then
  echo "Usage: $0 <vMAJOR.MINOR> | --self-test" >&2
  exit 2
fi

validate_tag "$1"
echo "Validated release tag $1 for Cargo package version $package_version."
