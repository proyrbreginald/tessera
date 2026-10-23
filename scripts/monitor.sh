#!/usr/bin/env bash
# 只看串口并检验时基漂移。
#
# 用法：scripts/monitor.sh [设备] [额外参数…]
# 默认设备取台架别名（见 scripts/tool-paths.sh）。**不要写死 ttyUSBn** ——
# 内核按枚举顺序分配编号，换 USB 口/插拔顺序/重启都会换号。
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/tool-paths.sh
source scripts/tool-paths.sh

python="$(resolve_pyocd_python)"
exec "$python" scripts/read_console.py "${1:-$UART_ALIAS}" 115200 "${@:2}"
