#!/bin/sh
# 编译 OneBotFilter（Rust，release）。
#
#   sh scripts/build.sh                                  # 当前主机
#   sh scripts/build.sh x86_64-unknown-linux-gnu         # Linux amd64
#   sh scripts/build.sh x86_64-pc-windows-msvc           # Windows amd64
#   sh scripts/build.sh aarch64-apple-darwin             # macOS arm64
#
# 产物落在 dist/。跨平台编译需要目标平台工具链，通常直接用 GitHub Release 的预编译包。
set -eu

TARGET=${1:-}
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
TARGET_DIR=${CARGO_TARGET_DIR:-target}

if [ -n "$TARGET" ]; then
    cargo build --release --target "$TARGET"
    SRC="$TARGET_DIR/$TARGET/release/onebotfilter"
    case "$TARGET" in
        x86_64-unknown-linux-gnu) SUFFIX=linux-amd64 ;;
        x86_64-pc-windows-msvc)   SRC="$SRC.exe"; SUFFIX=windows-amd64.exe ;;
        aarch64-apple-darwin)     SUFFIX=macos-arm64 ;;
        *)                        SUFFIX=$TARGET ;;
    esac
else
    cargo build --release
    SRC="$TARGET_DIR/release/onebotfilter"
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    ARCH=$(uname -m)
    SUFFIX="$OS-$ARCH"
    [ -f "$SRC.exe" ] && SRC="$SRC.exe"
fi

mkdir -p dist
cp "$SRC" "dist/onebotfilter-$SUFFIX"
printf 'built %s\n' "dist/onebotfilter-$SUFFIX"
