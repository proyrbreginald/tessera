#!/usr/bin/env bash
# 台架工具解析：把"工具在哪"集中成一处，避免每个脚本各写一份路径。
#
# 解析顺序（第一个可用者胜出）：
#   1. 环境变量覆盖：PYOCD / PYOCD_PY
#   2. 本项目的 .venv
#   3. 相邻项目的 .venv（台架工具统一在那里维护；路径可用 ALT_VENV 覆盖）
#   4. 系统 PATH
# 都找不到就报错并给出处置命令 —— 而不是让调用方在别处看到含糊的 "command not found"。

# 「相邻项目的 venv」是一个**环境事实**（本机台架工具的共享位置），不是本项目的依赖：
# 换机器时用 ALT_VENV 覆盖，或干脆 `make setup` 用本项目自己的 .venv。
ALT_VENV="${ALT_VENV:-/home/reginald/project/elevator-protocol-gateway/.venv}"

resolve_pyocd() {
    if [ -n "${PYOCD:-}" ] && [ -x "${PYOCD}" ]; then printf '%s' "$PYOCD"; return 0; fi
    for c in "./.venv/bin/pyocd" "$ALT_VENV/bin/pyocd"; do
        [ -x "$c" ] && { printf '%s' "$c"; return 0; }
    done
    command -v pyocd 2>/dev/null && return 0
    return 1
}

resolve_pyocd_python() {
    if [ -n "${PYOCD_PY:-}" ] && [ -x "${PYOCD_PY}" ]; then printf '%s' "$PYOCD_PY"; return 0; fi
    local pyocd; pyocd="$(resolve_pyocd)" || return 1
    # pyocd 与 python 在同一 venv 里；取其同级解释器。
    local dir; dir="$(dirname "$pyocd")"
    [ -x "$dir/python" ] && { printf '%s' "$dir/python"; return 0; }
    command -v python3 2>/dev/null && return 0
    return 1
}

# 台架设备别名（由 udev 统一维护；规则源在台架工具目录下）
UART_ALIAS="${UART_ALIAS:-/dev/pudu-debugger-16p-uart}"
SWD_ALIAS="${SWD_ALIAS:-/dev/pudu-debugger-swd}"

require_pyocd() {
    local pyocd
    if ! pyocd="$(resolve_pyocd)"; then
        cat >&2 <<'MSG'
[台架] 找不到 pyocd。处置（任选其一）：
    make setup                     # 在本项目建 .venv 并安装 pyocd
    PYOCD=/path/to/pyocd make flash    # 显式指定
MSG
        return 1
    fi
    printf '%s' "$pyocd"
}
