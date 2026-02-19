#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if [[ ! -f RELEASE ]]; then
    echo "Error: RELEASE file not found in $SCRIPT_DIR" >&2
    exit 1
fi

current_rev="$(tr -d '[:space:]' < RELEASE)"
if [[ -z "$current_rev" || ! "$current_rev" =~ ^[0-9]+$ ]]; then
    echo "Error: RELEASE must contain a numeric revision. Found: '$current_rev'" >&2
    exit 1
fi

next_rev="$((current_rev + 1))"
printf '%s\n' "$next_rev" > RELEASE
echo "Release bumped: $current_rev -> $next_rev"

./build-rpm.sh

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
if [[ -z "$version" ]]; then
    echo "Error: Could not read version from Cargo.toml" >&2
    exit 1
fi

dist="$(rpm --eval '%dist' | sed 's/^\.//' | tr -d '\n')"
if [[ -z "$dist" ]]; then
    dist="fc40"
fi

rpm_path="./x86_64/ibus-dikt-${version}-${next_rev}.${dist}.x86_64.rpm"
if [[ ! -f "$rpm_path" ]]; then
    rpm_path="$(find . -maxdepth 3 -type f -name "ibus-dikt-${version}-${next_rev}.${dist}*.x86_64.rpm" | head -n1 || true)"
fi

if [[ -z "$rpm_path" || ! -f "$rpm_path" ]]; then
    echo "Error: Built RPM not found for version=${version} release=${next_rev} dist=${dist}" >&2
    exit 1
fi

echo "Installing: $rpm_path"
sudo dnf install -y "$rpm_path"

echo "Restarting dikt daemon and IBus..."
systemctl --user restart dikt.service
ibus restart

echo "Done."
echo "Installed: $rpm_path"
echo "Current RELEASE: $next_rev"
