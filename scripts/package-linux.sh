#!/usr/bin/env bash
set -euo pipefail

# Portable Linux x64 tarball + AppDir packager for square-to-search.
# Enforces Decision 3A (zero CUDA/cuDNN/cuBLAS/NCCL shared libraries).

SKIP_BUILD=0
OUT_DIR="dist"

usage() {
    cat <<'EOF'
Usage: scripts/package-linux.sh [--skip-build] [--out-dir <dir>] [out-dir]

Options:
  --skip-build        Skip `cargo build --release -p sts-app`
  --out-dir <dir>     Output directory (default: dist)
  -h, --help          Show this help message
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        --out-dir)
            if [ "$#" -lt 2 ]; then
                echo "Error: --out-dir requires a path argument" >&2
                exit 1
            fi
            OUT_DIR="$2"
            shift 2
            ;;
        --out-dir=*)
            OUT_DIR="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "Error: unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
        *)
            OUT_DIR="$1"
            shift
            ;;
    esac
done

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"

CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
if [ -d "$CARGO_BIN" ]; then
    case ":$PATH:" in
        *":$CARGO_BIN:"*) ;;
        *) export PATH="$CARGO_BIN:$PATH" ;;
    esac
fi

case "$OUT_DIR" in
    /*) RESOLVED_OUT_DIR="$OUT_DIR" ;;
    *) RESOLVED_OUT_DIR="$REPO_ROOT/$OUT_DIR" ;;
esac

RELEASE_DIR="$REPO_ROOT/target/release"
PKG_NAME="square-to-search-linux-x64"
STAGE_ROOT="$RESOLVED_OUT_DIR/staging"
TAR_STAGE_DIR="$STAGE_ROOT/$PKG_NAME"
APPDIR="$RESOLVED_OUT_DIR/AppDir"
TAR_NAME="${PKG_NAME}.tar.gz"
TAR_PATH="$RESOLVED_OUT_DIR/$TAR_NAME"
SHA_PATH="${TAR_PATH}.sha256"

CUDA_SO_REGEX='(cuda|cudnn|cublas|cufft|curand|cusolver|cusparse|nccl|nvrtc)'

check_no_cuda_sos() {
    local search_dir="$1"
    local label="$2"
    if [ ! -d "$search_dir" ]; then
        return 0
    fi
    while IFS= read -r -d '' so_file; do
        local base_name
        base_name="$(basename -- "$so_file")"
        if printf '%s' "$base_name" | grep -Eqi "$CUDA_SO_REGEX"; then
            echo "Decision 3A violation in ${label}: forbidden CUDA library detected: $so_file" >&2
            exit 1
        fi
    done < <(find "$search_dir" -maxdepth 1 \( -type f -o -type l \) -name '*.so*' -print0)
}

cd "$REPO_ROOT"

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "[package-linux] Building release binary: cargo build --release -p sts-app"
    cargo build --release -p sts-app
fi

BIN_PATH="$RELEASE_DIR/sts-app"
if [ ! -f "$BIN_PATH" ] || [ ! -s "$BIN_PATH" ]; then
    echo "Error: release binary missing or empty at '$BIN_PATH'" >&2
    exit 1
fi

README_PATH="$REPO_ROOT/README.md"
if [ ! -f "$README_PATH" ]; then
    echo "Error: README.md missing at '$README_PATH'" >&2
    exit 1
fi

# Decision 3A guard before staging.
check_no_cuda_sos "$RELEASE_DIR" "target/release"

rm -rf -- "$TAR_STAGE_DIR" "$APPDIR"
mkdir -p -- "$TAR_STAGE_DIR/lib" "$APPDIR/usr/bin" "$APPDIR/usr/lib" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/scalable/apps"

cp -L -- "$BIN_PATH" "$TAR_STAGE_DIR/sts-app"
cp -L -- "$BIN_PATH" "$APPDIR/usr/bin/sts-app"
chmod 0755 -- "$TAR_STAGE_DIR/sts-app" "$APPDIR/usr/bin/sts-app"
cp -L -- "$README_PATH" "$TAR_STAGE_DIR/README.md"

# Stage any non-CUDA shared libraries (including symlinked libonnxruntime*.so*) dereferenced via cp -L.
while IFS= read -r -d '' so_file; do
    base_name="$(basename -- "$so_file")"
    cp -L -- "$so_file" "$TAR_STAGE_DIR/lib/$base_name"
    cp -L -- "$so_file" "$APPDIR/usr/lib/$base_name"
    if [ ! -s "$TAR_STAGE_DIR/lib/$base_name" ] || [ ! -s "$APPDIR/usr/lib/$base_name" ]; then
        echo "Error: staged shared library '$base_name' is empty or broken symlink" >&2
        exit 1
    fi
done < <(find "$RELEASE_DIR" -maxdepth 1 \( -type f -o -type l \) -name '*.so*' -print0)

# Decision 3A guard on staged libraries.
check_no_cuda_sos "$TAR_STAGE_DIR/lib" "staging/lib"
check_no_cuda_sos "$APPDIR/usr/lib" "AppDir/usr/lib"

# Launcher wrapper for portable tarball.
cat > "$TAR_STAGE_DIR/square-to-search" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
HERE="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
export LD_LIBRARY_PATH="$HERE/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$HERE/sts-app" "$@"
EOF
chmod 0755 -- "$TAR_STAGE_DIR/square-to-search"

# Desktop entry, minimal SVG icon, and AppRun wrapper for AppDir / AppImageTool.
cat > "$APPDIR/square-to-search.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Square to Search
Comment=Snip-to-OCR, Reverse Image Search, Ask AI, and Continuous Manga Live Translate
Exec=sts-app
Icon=square-to-search
Terminal=false
Categories=Utility;Graphics;
StartupNotify=false
EOF
cp -- "$APPDIR/square-to-search.desktop" "$APPDIR/usr/share/applications/square-to-search.desktop"

cat > "$APPDIR/square-to-search.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128">
  <rect width="128" height="128" rx="24" fill="#1d4ed8"/>
  <rect x="26" y="26" width="76" height="76" rx="12" fill="none" stroke="#ffffff" stroke-width="10" stroke-dasharray="18 10"/>
  <circle cx="88" cy="88" r="14" fill="#38bdf8"/>
</svg>
EOF
cp -- "$APPDIR/square-to-search.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/square-to-search.svg"

cat > "$APPDIR/AppRun" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
HERE="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$HERE/usr/bin/sts-app" "$@"
EOF
chmod 0755 -- "$APPDIR/AppRun"

rm -f -- "$TAR_PATH" "$SHA_PATH"
tar -C "$STAGE_ROOT" -czf "$TAR_PATH" "$PKG_NAME"

if [ ! -s "$TAR_PATH" ]; then
    echo "Error: failed to create tarball '$TAR_PATH'" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$RESOLVED_OUT_DIR" && sha256sum "$TAR_NAME" > "${TAR_NAME}.sha256")
elif command -v shasum >/dev/null 2>&1; then
    (cd "$RESOLVED_OUT_DIR" && shasum -a 256 "$TAR_NAME" > "${TAR_NAME}.sha256")
else
    echo "Error: neither sha256sum nor shasum found on PATH" >&2
    exit 1
fi

echo "[package-linux] Done: $TAR_PATH"
echo "[package-linux] AppDir staged at: $APPDIR"
echo "[package-linux] SHA-256: $(cat "$SHA_PATH")"
