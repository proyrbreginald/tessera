#!/usr/bin/env bash
# 控制台**逐字节**校验：复位板子、抓启动输出、逐条比对期望文本。
#
# 用法：scripts/bytecheck.sh [--seconds N] [--port /dev/…]
# 默认端口取台架别名（见 scripts/tool-paths.sh）。**不要写死 ttyUSBn**。
#
# 为什么需要它：肉眼不可靠 —— 中断发送丢字节时，长输出的**后半段**才会烂，
# 前几行看起来完全正常。判据必须是"剥离 ANSI 后逐条比对期望文本"。
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/tool-paths.sh
source scripts/tool-paths.sh

# 解释器与 pyocd 同在一个 venv（pyserial 随 pyocd 装好），
# 因此**必须**用它而不是系统 python3 —— 后者往往没有 pyserial。
python="$(resolve_pyocd_python)"
exec "$python" scripts/console-bytecheck.py "$@"
