#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="codex-forge"
BUILD_BIN="$SCRIPT_DIR/target/release/$BIN_NAME"
INSTALL_PATH="/usr/local/bin/$BIN_NAME"

cd "$SCRIPT_DIR"

echo "开始构建 $BIN_NAME ..."
cargo build --release

if [[ ! -x "$BUILD_BIN" ]]; then
  echo "未找到构建产物：$BUILD_BIN" >&2
  exit 1
fi

if [[ -w "$(dirname "$INSTALL_PATH")" ]]; then
  install -m 755 "$BUILD_BIN" "$INSTALL_PATH"
else
  sudo install -m 755 "$BUILD_BIN" "$INSTALL_PATH"
fi

echo "已安装到：$INSTALL_PATH"
"$INSTALL_PATH" --version
