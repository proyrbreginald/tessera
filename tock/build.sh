#!/usr/bin/env bash
# 构建「用 Tock 替换 RTIC」的最小可跑验证镜像（实验性，不属于生产固件）。
#
# 依赖：外部 Tock 2.3.0 源码树（本仓库不内联 49 MB 的 Tock）。
#   TOCK_DIR=/path/to/tock   bash tock/build.sh
# 默认路径：tmp/tock-release-2.3.0（交接时提供的源码位置；tmp/ 不入库）。
#
# 本脚本会（幂等地）：
#   1. 把 tock/boards/tessera-rt1021/ 复制进 Tock 工作区；
#   2. 在 Tock 的 Cargo.toml members 里登记该板；
#   3. 对 chips/imxrt10xx/src/lpuart.rs 应用 2 个 TESSERA-PATCH；
#   4. 用 stable 工具链构建并打印 ELF 路径。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOCK_DIR="${TOCK_DIR:-$ROOT/tmp/tock-release-2.3.0}"
BOARD_SRC="$ROOT/tock/boards/tessera-rt1021"
BOARD_DST="$TOCK_DIR/boards/tessera-rt1021"
PATCH_DIR="$ROOT/tock/boards/tessera-rt1021/tock-patches"

if [ ! -f "$TOCK_DIR/Cargo.toml" ]; then
    echo "找不到 Tock 源码树：$TOCK_DIR" >&2
    echo "请把 Tock 2.3.0 解压到该路径，或设置 TOCK_DIR 指向实际位置。" >&2
    exit 2
fi

# 1) 复制板级 crate（覆盖旧副本，保证仓库内版本是唯一真源）。
rm -rf "$BOARD_DST"
mkdir -p "$TOCK_DIR/boards"
cp -r "$BOARD_SRC" "$BOARD_DST"
echo "[tock] 板级 crate → $BOARD_DST"

# 2) 登记 workspace 成员（幂等）。
if ! grep -q 'boards/tessera-rt1021' "$TOCK_DIR/Cargo.toml"; then
    python3 - "$TOCK_DIR/Cargo.toml" <<'PY'
import sys
from pathlib import Path
path = Path(sys.argv[1])
text = path.read_text()
needle = '    "boards/imxrt1050-evkb",\n'
if needle not in text:
    raise SystemExit("[tock] Cargo.toml 里找不到 imxrt1050-evkb 成员行")
text = text.replace(needle, needle + '    "boards/tessera-rt1021",\n', 1)
path.write_text(text)
PY
    echo "[tock] 已登记 workspace 成员"
else
    echo "[tock] workspace 成员已存在"
fi

# 3) 应用 lpuart 补丁（幂等：已应用则跳过）。
apply_patch() {
    local patch_file="$1"
    if patch -d "$TOCK_DIR" -p1 --forward --dry-run < "$patch_file" >/dev/null 2>&1; then
        patch -d "$TOCK_DIR" -p1 --forward < "$patch_file" >/dev/null
        echo "[tock] 已应用 $(basename "$patch_file")"
    elif patch -d "$TOCK_DIR" -p1 --reverse --dry-run < "$patch_file" >/dev/null 2>&1; then
        echo "[tock] 已应用过（跳过）$(basename "$patch_file")"
    else
        echo "[tock] 补丁既不适用也非已应用：$(basename "$patch_file")" >&2
        echo "      请确认 Tock 源码是 2.3.0 原始版本。" >&2
        exit 3
    fi
}
for patch_file in "$PATCH_DIR"/*.patch; do
    apply_patch "$patch_file"
done

# 4) 构建（stable；Tock 默认的 nightly flags 已在板级 .cargo/config.toml 去掉）。
cd "$BOARD_DST"
RUSTUP_TOOLCHAIN=stable cargo build --release

ELF="$TOCK_DIR/target/thumbv7em-none-eabihf/release/tessera-rt1021"
echo
echo "[tock] 构建完成：$ELF"
echo "[tock] 烧录（全片擦除后写入）："
echo "       ./.venv/bin/pyocd load --target mimxrt1020 --format elf -e chip \"$ELF\""
