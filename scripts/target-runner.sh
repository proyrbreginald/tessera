#!/usr/bin/env bash
# Cargo `runner`：目标端产物无法在主机上执行。
#
# 存在这个脚本的意义是把内核那句含糊的 "exec format error" 换成一条能读懂的信息。
# 目标端（HIL）测试需要真实硬件，见 docs/04-开发/测试策略.md 的 L6 层。
set -euo pipefail
cat >&2 <<'MSG'
[runner] 这是目标端（thumbv7em-none-eabihf）产物，不能在主机上运行。

主机测试请用：
    make test          # 等价于 cargo test --target x86_64-unknown-linux-gnu -p <纯逻辑层>

目标端（真机在环）测试尚未接线；上机验证当前由 scripts 下的台架流程承担：
    make check-bench / make flash / make monitor / make bytecheck
MSG
exit 1
