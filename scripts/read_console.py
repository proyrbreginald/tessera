#!/usr/bin/env python3
"""读串口并检验时基漂移。

为什么值得单独写一个脚本：串口能出字**只**证明波特率和引脚对了；
真正检验时钟树的是设备自己上报的 uptime —— 它取自晶振时基，
与串口本身无关。因此本脚本的判据是"设备时间 vs 主机时间"。
"""
from __future__ import annotations

import argparse
import re
import sys
import time

try:
    import serial  # type: ignore
except ImportError:
    print("缺少 pyserial：make setup", file=sys.stderr)
    raise SystemExit(2)

BEAT = re.compile(r"uptime=(\d+)\.(\d{3})s")
TOLERANCE_PERCENT = 2.0


def main() -> int:
    parser = argparse.ArgumentParser(description="读取串口并检验时基漂移")
    parser.add_argument("device", help="串口设备（请用 udev 别名，不要用 ttyUSBn）")
    parser.add_argument("baud", type=int, nargs="?", default=115200)
    parser.add_argument("--seconds", type=float, default=10.0, help="观察时长")
    parser.add_argument(
        "--no-beat",
        action="store_true",
        help="不发送 `beat on`（默认会发送，以便采集心跳行做时基漂移判定）",
    )
    args = parser.parse_args()

    lines = 0
    beat_enabled = False
    # 最近两次心跳的 `(设备时间, 主机时间)`。漂移用**相邻样本的增量**计算，
    # 不用"第一个样本 vs 现在"：抓包开始时串口缓冲里可能积压了几行旧心跳，
    # 拿它当基准会把正常时钟判成 +9%（2026-10-19 真机首次校准时踩到）。
    samples: list[tuple[float, float]] = []
    violations = 0

    with serial.Serial(args.device, args.baud, timeout=1) as port:
        # 固件默认**不主动输出心跳**（终端就绪后只等待输入）。
        # 本脚本需要 `uptime` 来判定时基漂移，因此打开串口后立刻显式开启；
        # 不能等"纯逻辑终端"那行 —— 启动很快时（0.25.x 约 1 s）那一行可能已经
        # 在脚本打开串口前打印完，等它就永远等不到心跳。
        # 打开后立刻写入也不怕早：终端未就绪时字节会留在 RX 队列，就绪后处理。
        beat_enabled = False
        if not args.no_beat:
            port.write(b"beat on\n")
            beat_enabled = True
        deadline = time.monotonic() + args.seconds
        try:
            while time.monotonic() < deadline:
                raw = port.readline()
                if not raw:
                    continue
                text = raw.decode("utf-8", errors="replace").rstrip()
                print(text)
                lines += 1
                match = BEAT.search(text)
                if match:
                    device_time = int(match.group(1)) + int(match.group(2)) / 1000.0
                    samples.append((device_time, time.monotonic()))
                    # 跳过前 2 条心跳（第 1 个区间常被启动输出/串口缓冲拉长）。
                    if len(samples) >= 3:
                        (device_prev, host_prev), (device_now, host_now) = samples[-2], samples[-1]
                        host_delta = host_now - host_prev
                        if host_delta <= 0.2:
                            continue
                        drift = ((device_now - device_prev) - host_delta) / host_delta * 100.0
                        if abs(drift) <= TOLERANCE_PERCENT:
                            verdict = "OK"
                        else:
                            verdict = "超差"
                            violations += 1
                        print(f"  → 漂移 {drift:+.3f} % （容差 ±{TOLERANCE_PERCENT}%）{verdict}")
        finally:
            if beat_enabled:
                port.write(b"beat off\n")

    if lines == 0:
        print("没有读到任何行：检查波特率、接线与复位。", file=sys.stderr)
        return 1
    if not args.no_beat and len(samples) < 3:
        print(
            f"只读到 {len(samples)} 条心跳：无法判定时基漂移（需要 ≥3 条，前 2 条用于预热）。",
            file=sys.stderr,
        )
        return 1
    if violations > 0:
        print(
            f"时基漂移超差：{violations} 次（容差 ±{TOLERANCE_PERCENT}%）。",
            file=sys.stderr,
        )
        return 1
    if len(samples) >= 3:
        print(f"时基漂移判定：{len(samples)} 条心跳（前 2 条预热）全部在 ±{TOLERANCE_PERCENT}% 内。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
