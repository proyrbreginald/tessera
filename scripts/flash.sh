#!/usr/bin/env bash
# 烧录到外部 QSPI NOR（先跑台架门禁）。
#
# 为什么"烧录成功"本身就是证据：探针不能隔着 SWD 直写 QSPI。真实流程是探针把一小段
# flash 算法下进片上 RAM，让它初始化 FlexSPI 后再写 —— 用的读命令 / dummy 周期 /
# 时钟与 Boot ROM 完全相同。因此能烧进去就说明 FCB 的关键参数是对的。
#
# ⚠️ 若出现 `flash init timed out`，说明核心**根本没有在执行指令**
#    （2026-10-09 实测遇到：被摁在复位态 —— 内存写入不落地、PC 冻结、
#      外设与 Flash 空间全不可访问）。那是硬件状态问题：先断电恢复，
#      不要反复重试，重试不会让它变好。
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/tool-paths.sh
source scripts/tool-paths.sh

bash scripts/check-bench.sh
pyocd="$(require_pyocd)"
cargo build --release
"$pyocd" load --target mimxrt1020 --format elf "$@" \
    target/thumbv7em-none-eabihf/release/firmware
echo "✅ 已烧录。注意：烧录成功 ≠ 启动成功 —— 镜像必须位于 Flash 偏移 0。"
