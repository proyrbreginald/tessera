#!/usr/bin/env bash
# 证据账本：把一次真机/HIL 运行的"版本 × 镜像 × 板卡 × 结果"登记到 tmp/evidence/。
#
# 为什么需要它：项目里最贵的不是某条测试，而是"这条结论是对哪个镜像、哪块板、
# 哪次运行成立的"这个上下文。没有账本，"0.21 已上机/未上机"这种判断只能靠记忆。
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(cat VERSION)"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="tmp/evidence/${STAMP}-v${VERSION}"
mkdir -p "$OUT"

{
    echo "recorded_at: $(date -Iseconds)"
    echo "version: ${VERSION}"
    echo "git_commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "rustc: $(rustc -vV | sed -n '1p')"
    echo "elf_sha256: $(sha256sum target/thumbv7em-none-eabihf/release/firmware 2>/dev/null | cut -d' ' -f1 || echo missing)"
    echo "probe:"
    .venv/bin/pyocd list 2>&1 || true
    echo "bench_baseline:"
    cat bench/baseline.json 2>/dev/null || true
} > "${OUT}/manifest.txt"

# 已有的测试/覆盖率产物随账本归档（存在才复制）。
for file in tmp/host-tests.json tmp/coverage-summary.json; do
    if [ -f "${file}" ]; then
        cp "${file}" "${OUT}/"
    fi
done

# 抓一段启动输出（先复位一次；不改变固件）。串口别名缺失时只记录失败，不阻断。
# 为什么必须复位：终端就绪后固件**默认静默**（只等输入），不复位就没有可抓的横幅。
UART="${UART_ALIAS:-/dev/pudu-debugger-16p-uart}"
if [ -e "${UART}" ]; then
    .venv/bin/pyocd reset -t mimxrt1020 >/dev/null 2>&1 || true
    sleep 1
    timeout 30 .venv/bin/python scripts/read_console.py "${UART}" --seconds 12 \
        > "${OUT}/monitor.txt" 2>&1 || echo "monitor 非 0（见 ${OUT}/monitor.txt）"
else
    echo "串口 ${UART} 不存在，跳过 monitor 抓包" > "${OUT}/monitor.txt"
fi

echo "证据目录：${OUT}"
echo "  manifest: ${OUT}/manifest.txt"
echo "  monitor : ${OUT}/monitor.txt"
