#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_DIR="$DATA_DIR/icons/hicolor/scalable/apps"
APP_DIR="$DATA_DIR/applications"

echo "==> 编译 Release 版本..."
cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"

echo "==> 安装可执行文件到 $BIN_DIR/pcstui"
mkdir -p "$BIN_DIR" "$ICON_DIR" "$APP_DIR"
install -m 0755 "$ROOT_DIR/target/release/pcstui" "$BIN_DIR/pcstui"

echo "==> 安装图标到 $ICON_DIR/pcstui.svg"
install -m 0644 "$ROOT_DIR/assets/pcstui.svg" "$ICON_DIR/pcstui.svg"

echo "==> 安装桌面启动器到 $APP_DIR/pcstui.desktop"
if [ -n "${PCSTUI_HOME:-}" ]; then
    EXEC_LINE="Exec=env PCSTUI_HOME=$PCSTUI_HOME $BIN_DIR/pcstui"
else
    EXEC_LINE="Exec=$BIN_DIR/pcstui"
fi
sed "s|^Exec=.*|$EXEC_LINE|" "$ROOT_DIR/assets/pcstui.desktop" > "$APP_DIR/pcstui.desktop"
chmod 0644 "$APP_DIR/pcstui.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$DATA_DIR/icons/hicolor" >/dev/null 2>&1 || true
fi
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 >/dev/null 2>&1 || true
fi

echo "==> 完成！现在可以在终端运行 pcstui，或在系统开始菜单中搜索 PcsTui 启动。"
