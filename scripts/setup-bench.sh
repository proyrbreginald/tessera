#!/usr/bin/env bash
# 建 Python 虚拟环境并装烧录/串口工具（无需 root）。
set -euo pipefail
cd "$(dirname "$0")/.."
if [ ! -d .venv ]; then
    python3 -m venv .venv
fi
./.venv/bin/pip install --disable-pip-version-check -q pyocd pyserial
./.venv/bin/pyocd --version
echo "✅ 台架工具就绪：.venv/bin/pyocd、.venv/bin/python"
