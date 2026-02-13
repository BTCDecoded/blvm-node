#!/usr/bin/env bash
# Install TidesDB C library for blvm-node tidesdb backend
# Without sudo: installs to TIDESDB_PREFIX (default: ~/.local)
# With sudo: pass --system to install to /usr/local

set -e

PREFIX="${TIDESDB_PREFIX:-$HOME/.local}"
SYSTEM_INSTALL=false

if [[ "$1" == "--system" ]]; then
    SYSTEM_INSTALL=true
    PREFIX="/usr/local"
fi

echo "TidesDB install prefix: $PREFIX"
mkdir -p "$PREFIX"

BUILD_DIR="${TMPDIR:-/tmp}/tidesdb-build-$$"
trap "rm -rf '$BUILD_DIR'" EXIT
mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"

echo "Cloning tidesdb..."
git clone --depth 1 https://github.com/tidesdb/tidesdb.git
cd tidesdb

echo "Building..."
cmake -S . -B build -DCMAKE_INSTALL_PREFIX="$PREFIX"
cmake --build build

if $SYSTEM_INSTALL; then
    echo "Installing to $PREFIX (requires sudo)..."
    sudo cmake --install build
else
    echo "Installing to $PREFIX..."
    cmake --install build --prefix "$PREFIX"
fi

# Create pkg-config file if not present (tidesdb CMake doesn't create one)
PKG_DIR="$PREFIX/lib/pkgconfig"
mkdir -p "$PKG_DIR"
if [[ ! -f "$PKG_DIR/tidesdb.pc" ]]; then
    cat > "$PKG_DIR/tidesdb.pc" << EOF
prefix=$PREFIX
exec_prefix=\${prefix}
libdir=\${exec_prefix}/lib
includedir=\${prefix}/include

Name: tidesdb
Description: TidesDB LSM-tree key-value storage engine
Version: 8.3.2
Libs: -L\${libdir} -ltidesdb
Cflags: -I\${includedir}
EOF
    echo "Created $PKG_DIR/tidesdb.pc"
fi

echo ""
echo "TidesDB installed to $PREFIX"
echo ""
echo "To build blvm-node with TidesDB:"
echo "  export PKG_CONFIG_PATH=\"$PREFIX/lib/pkgconfig:\$PKG_CONFIG_PATH\""
echo "  export LD_LIBRARY_PATH=\"$PREFIX/lib:\$LD_LIBRARY_PATH\""
echo "  cargo build -p blvm-node"
echo ""
echo "Or add to ~/.bashrc for persistence:"
echo "  export PKG_CONFIG_PATH=\"$PREFIX/lib/pkgconfig:\$PKG_CONFIG_PATH\""
echo "  export LD_LIBRARY_PATH=\"$PREFIX/lib:\$LD_LIBRARY_PATH\""
