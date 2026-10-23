#!/usr/bin/env bash
# 全量离线验证的唯一入口。
#
# 之所以只做"转发"：验证逻辑必须只有一份。分散在 Makefile、CI、脚本里的三份
# 一定会漂移，而漂移的验证等于没有验证。
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== 工具链 =="
rustc -vV | sed -n '1p'
rustup target list --installed | sed 's/^/  target: /'
echo
exec cargo xtask verify
