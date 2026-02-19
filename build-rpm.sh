#!/bin/bash
set -e

# Single source of truth - read version from Cargo.toml and release from RELEASE file
VERSION=$(grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
RELEASE=$(cat RELEASE)
DIST="$(rpm --eval '%dist' | sed 's/^\.//' | tr -d '\n')"
if [ -z "$DIST" ]; then
    DIST="fc40"
fi

echo "=== Building ibus-dikt ${VERSION}-${RELEASE}.${DIST} ==="

# Check for required commands
command -v cargo >/dev/null 2>&1 || { echo "Error: cargo not found. Please install Rust."; exit 1; }
command -v rpmbuild >/dev/null 2>&1 || { echo "Error: rpmbuild not found. Please install rpm-build."; exit 1; }
command -v rsync >/dev/null 2>&1 || { echo "Error: rsync not found. Please install rsync."; exit 1; }

# Get script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Generate spec file from template
echo "Generating spec file from template..."
sed -e "s/@VERSION@/$VERSION/g" \
    -e "s/@RELEASE@/$RELEASE/g" \
    packaging/fedora/ibus-dikt.spec.in > packaging/fedora/ibus-dikt.spec
echo "Generated packaging/fedora/ibus-dikt.spec"

# Generate dikt.xml from template
echo "Generating dikt.xml from template..."
sed -e "s/@VERSION@/$VERSION/g" \
    packaging/fedora/dikt.xml.in > packaging/fedora/dikt.xml
echo "Generated packaging/fedora/dikt.xml"

# Create source tarball
echo "Creating source tarball..."
TARBALL="ibus-dikt-${VERSION}.tar.gz"
STAGE_DIR="$(mktemp -d)"
mkdir -p "${STAGE_DIR}/ibus-dikt-${VERSION}"
rsync -a \
    --exclude='.git' \
    --exclude='target' \
    --exclude='x86_64' \
    --exclude='*.rpm' \
    --exclude='*.src.rpm' \
    --exclude='ibus-dikt-*.tar.gz' \
    ./ "${STAGE_DIR}/ibus-dikt-${VERSION}/"
tar -C "${STAGE_DIR}" -czf "$TARBALL" "ibus-dikt-${VERSION}"
rm -rf "${STAGE_DIR}"
echo "Created $TARBALL"

# Build SRPM
echo ""
echo "=== Building SRPM ==="
rpmbuild -bs \
    --define "_sourcedir $(pwd)" \
    --define "_specdir $(pwd)" \
    --define "_srcrpmdir $(pwd)" \
    --define "_rpmdir $(pwd)" \
    packaging/fedora/ibus-dikt.spec

SRPM=$(ls ibus-dikt-${VERSION}-${RELEASE}*.src.rpm 2>/dev/null | head -1)
if [ -n "$SRPM" ]; then
    echo "Created SRPM: $SRPM"
else
    echo "Error: SRPM not found"
    exit 1
fi

# Build RPM
echo ""
echo "=== Building RPM ==="
rpmbuild -bb \
    --define "_sourcedir $(pwd)" \
    --define "_specdir $(pwd)" \
    --define "_srcrpmdir $(pwd)" \
    --define "_rpmdir $(pwd)" \
    --define "dist .${DIST}" \
    packaging/fedora/ibus-dikt.spec

echo ""
echo "=== Build Complete ==="
echo "SRPM: $SRPM"
echo "RPM packages:"
find . -name "ibus-dikt-${VERSION}-${RELEASE}.${DIST}*.rpm" -type f 2>/dev/null | while read rpm; do
    echo "  - $rpm"
done

echo ""
echo "To install:"
echo "  sudo dnf install ./ibus-dikt-${VERSION}-${RELEASE}.${DIST}.x86_64.rpm"
