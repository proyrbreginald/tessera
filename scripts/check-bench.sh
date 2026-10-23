#!/usr/bin/env bash
# 台架就绪检查：烧录之前必须通过。
#
# 设计取舍（与既有工程一致）：**确凿的硬前置条件**失败即退出（探针不在、节点不可读写、
# 没有烧录工具）；串口别名缺失只警告 —— 别让固件工作卡在与固件无关的环境问题上。
# 因此别名缺失**不能**当作环境 OK 的证明。
set -uo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/tool-paths.sh
source scripts/tool-paths.sh

fail=0
warn=0

echo "== 1. 调试探针（CMSIS-DAP c251:f001）=="
if lsusb 2>/dev/null | grep -q 'c251:f001'; then
    echo "  ✅ lsusb 列出了探针"
    # ⚠️ lsusb 能列出**不能**说明可用：libusb 与 lsusb 都靠 sysfs 枚举，
    # 即使 /dev 节点不存在也能列出来。必须验证节点可读写。
    node="$(lsusb | awk '/c251:f001/{print $2"/"substr($4,1,3)}' | head -1)"
    if [ -n "$node" ] && [ -r "/dev/bus/usb/$node" ] && [ -w "/dev/bus/usb/$node" ]; then
        echo "  ✅ /dev/bus/usb/$node 可读写"
    else
        echo "  ❌ /dev/bus/usb/${node:-?} 不存在或不可读写"
        echo "     处置：确认当前用户在 dialout 组（改后必须重新登录）；"
        echo "           且不是跑在重建了 /dev 的沙箱里（bwrap --dev /dev 无解）。"
        fail=1
    fi
else
    echo "  ❌ 未发现探针"
    echo "     注意：这不是唯一原因 —— 探针可能被占用、或 /dev 不可见。"
    fail=1
fi

echo
echo "== 2. SWD 节点别名 =="
if [ -e "$SWD_ALIAS" ]; then
    resolved="$(readlink -f "$SWD_ALIAS")"
    if [ -r "$resolved" ] && [ -w "$resolved" ]; then
        echo "  ✅ $SWD_ALIAS → $resolved 可读写"
    else
        echo "  ❌ $SWD_ALIAS 指向的 $resolved 不可读写"
        echo "     处置：确认当前用户在 dialout 组（改后必须重新登录）。"
        fail=1
    fi
else
    echo "  ❌ 缺少 $SWD_ALIAS"
    fail=1
fi

echo
echo "== 3. 调试串口别名 =="
if [ -e "$UART_ALIAS" ]; then
    if [ -r "$UART_ALIAS" ] && [ -w "$UART_ALIAS" ]; then
        echo "  ✅ $UART_ALIAS 可读写"
    else
        echo "  ❌ 别名存在但不可读写（组权限问题）"
        fail=1
    fi
else
    echo "  ⚠️  缺少 $UART_ALIAS（只警告，不阻断烧录）"
    echo "     别名由台架统一维护（路径解析见 scripts/tool-paths.sh）。"
    echo "     ⚠️ 别名缺失**不能**当作环境 OK 的证明。"
    warn=1
fi

echo
echo "== 4. 烧录工具 =="
if pyocd_path="$(resolve_pyocd)"; then
    echo "  ✅ pyocd: $pyocd_path"
else
    echo "  ❌ 找不到 pyocd（make setup，或用 PYOCD=... 指定）"
    fail=1
fi

echo
echo "== 5. 调试服务占用 =="
# 只匹配**真正的调试服务**：早先的宽匹配 `pyocd|openocd` 会命中调用它的那条命令行本身，
# 于是每次检查都误报"探针被占用"。
if pgrep -f 'pyocd (gdbserver|commander|erve)|openocd' >/dev/null 2>&1; then
    echo "  ⚠️  检测到正在运行的调试服务，可能占用探针："
    pgrep -a -f 'pyocd (gdbserver|commander|erve)|openocd' | sed 's/^/     /'
    warn=1
else
    echo "  ✅ 没有其它调试服务占用探针"
fi

echo
if [ "$fail" -ne 0 ]; then
    echo "台架门禁失败：不要烧录（失败信息指向的原因通常不是真正的原因）。"
    exit 1
fi
if [ "$warn" -ne 0 ]; then
    echo "台架检查通过（有警告，请阅读上面的 ⚠️ 项）"
else
    echo "台架检查全部通过。"
fi
exit 0
