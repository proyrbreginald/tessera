#!/usr/bin/env python3
"""目标端自检 runner（方案 A）：复位板子 → 发送 `selftest` → 解析逐条结果。

这不是 `cargo test` 的 std harness，而是板端 `selftest` 命令的串口 runner：
- 板端逐条打印 `test <name> ... ok|FAILED`，末尾 `TEST-END passed=N failed=M`；
- 主机只负责复位、发送、抓取、解析、退出码。

用法：
    make test-target
    .venv/bin/python scripts/target_test.py --timeout 60
"""
from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import time
from datetime import datetime

try:
    import serial  # type: ignore
except ImportError:
    print("缺少 pyserial：make setup", file=sys.stderr)
    raise SystemExit(2)

TEST_LINE = re.compile(r"^test\s+(\S+)\s+\.\.\.\s+(ok|FAILED)\s*$")
END_LINE = re.compile(r"^TEST-END passed=(\d+) failed=(\d+)\s*$")


def resolve_pyocd() -> str:
    for candidate in [os.environ.get("PYOCD"), "./.venv/bin/pyocd"]:
        if candidate and os.path.exists(candidate):
            return candidate
    return "pyocd"


def main() -> int:
    parser = argparse.ArgumentParser(description="目标端 selftest runner")
    parser.add_argument("--port", default=os.environ.get("UART_ALIAS", "/dev/pudu-debugger-16p-uart"))
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--out-dir", default="tmp/target-tests")
    args = parser.parse_args()

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    os.makedirs(args.out_dir, exist_ok=True)
    log_path = os.path.join(args.out_dir, f"{stamp}.log")

    subprocess.run(
        [resolve_pyocd(), "reset", "-t", "mimxrt1020"],
        capture_output=True,
        check=False,
    )

    results: list[tuple[str, bool]] = []
    end: tuple[int, int] | None = None

    with serial.Serial(args.port, 115200, timeout=0.5) as port, open(
        log_path, "w", encoding="utf-8"
    ) as log:
        # 等启动报告结束（终端就绪后默认静默）。
        boot_deadline = time.monotonic() + 8.0
        while time.monotonic() < boot_deadline:
            raw = port.readline()
            if not raw:
                continue
            text = raw.decode("utf-8", "replace").rstrip()
            log.write(text + "\n")
            if "纯逻辑终端" in text or "未实现项用" in text:
                break
        port.write(b"selftest\n")

        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            raw = port.readline()
            if not raw:
                continue
            text = raw.decode("utf-8", "replace").rstrip()
            log.write(text + "\n")
            match = TEST_LINE.match(text)
            if match:
                results.append((match.group(1), match.group(2) == "ok"))
                continue
            match = END_LINE.match(text)
            if match:
                end = (int(match.group(1)), int(match.group(2)))
                break

    for name, ok in results:
        print(f"  {'✅' if ok else '❌'} {name}")
    if end is None:
        print(f"target-test: 超时/未看到 TEST-END（日志 {log_path}）", file=sys.stderr)
        return 1
    passed, failed = end
    print(f"target-test: passed={passed} failed={failed}（日志 {log_path}）")
    if failed or passed != len(results) or any(not ok for _, ok in results):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
