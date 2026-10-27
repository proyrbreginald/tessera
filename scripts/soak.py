#!/usr/bin/env python3
"""长稳（soak）采集与判据：在真机上持续读心跳，统计复位/丢字节/实时违规/时基漂移。

用法：
    make soak                          # 默认 30 分钟
    SOAK_MINUTES=5 make soak           # 短稳验证脚本本身
    .venv/bin/python scripts/soak.py --minutes 60 --out-dir tmp/soak

判据（任一失败即退出码 1）：
- 运行期复位/故障；
- `txdrop` / `rxdrop` 非 0；
- `rt=VIOLATION` 或 `rt=none`（跳过前 3 条预热样本）；
- `dwt=DEAD`；
- （仅记录，不判失败）`covr=N`：维护步骤超 5 ms 预算的累计次数，见心跳行。
- 时基漂移超过 ±2%；
- 1 ms 节拍频率偏离 1000 Hz 超过 ±2%。

为什么单独写：`make monitor` 只看 10 秒；长期漂移、累积丢字节、偶发复位只有
在小时级窗口里才会出现。原始串口输出和 JSON 摘要都落到 `tmp/soak/`，便于归档。
"""
from __future__ import annotations

import argparse
import json
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

# 与固件 `report_beat` 的格式**逐字段对应**（2026-10-27 起带 `covr=`）。
# 历史上这里曾残留 `safe=`/`rst=` 两个固件从未输出过的字段，导致长稳一个样本都
# 匹配不到；字段增删必须同步本正则（这是一个没有门禁守护的跨语言契约）。
BEAT = re.compile(
    r"beat=(\d+) uptime=(\d+)\.(\d{3})s tick=(\d+) wdg_ok=(\d) "
    r"txdrop=(\d+) rxdrop=(\d+) wfi=(\d+) "
    r"dwt=(\w+) rearm=(\d+) fsq=(\d+)/(\d+)/(\d+)/(\d+) rt=(\w+) covr=(\d+)"
)
DRIFT_TOLERANCE_PERCENT = 2.0
TICK_RATE_TOLERANCE_PERCENT = 2.0
WARMUP_SAMPLES = 3


def resolve_pyocd() -> str:
    for candidate in [os.environ.get("PYOCD"), "./.venv/bin/pyocd"]:
        if candidate and os.path.exists(candidate):
            return candidate
    return "pyocd"


def main() -> int:
    parser = argparse.ArgumentParser(description="长稳采集与判据")
    parser.add_argument("--minutes", type=float, default=30.0)
    parser.add_argument("--port", default=os.environ.get("UART_ALIAS", "/dev/pudu-debugger-16p-uart"))
    parser.add_argument("--no-reset", action="store_true", help="不复位，直接观察当前运行中的板子")
    parser.add_argument("--out-dir", default="tmp/soak")
    args = parser.parse_args()

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    out_dir = os.path.join(args.out_dir, stamp)
    os.makedirs(out_dir, exist_ok=True)
    raw_path = os.path.join(out_dir, "console.txt")
    summary_path = os.path.join(out_dir, "summary.json")

    if not args.no_reset:
        subprocess.run(
            [resolve_pyocd(), "reset", "-t", "mimxrt1020"],
            capture_output=True,
            check=False,
        )
        time.sleep(1.0)

    samples: list[dict[str, int | str]] = []
    reboots = 0
    faults = 0
    beat_enabled = False
    deadline = time.monotonic() + args.minutes * 60.0
    start_host = time.monotonic()

    with open(raw_path, "w", encoding="utf-8") as raw, serial.Serial(args.port, 115200, timeout=1) as port:
        while time.monotonic() < deadline:
            line = port.readline()
            if not line:
                continue
            text = line.decode("utf-8", "replace").rstrip()
            raw.write(text + "\n")
            raw.flush()
            # 只认身份行；`timebase : ... · ...` 也含 "tessera " 与 "·"，曾造成误判。
            if text.startswith("tessera ") and "MIMXRT1021" in text:
                reboots += 1
            if "FAULT" in text:
                faults += 1
            if not beat_enabled and ("纯逻辑终端" in text or "未实现项用" in text):
                port.write(b"beat on\n")
                beat_enabled = True
            match = BEAT.search(text)
            if match:
                (
                    beat,
                    secs,
                    millis,
                    tick,
                    wdg_ok,
                    txdrop,
                    rxdrop,
                    wfi,
                    dwt,
                    rearm,
                    q_pending,
                    q_executed,
                    q_failed,
                    q_rejected,
                    rt,
                    covr,
                ) = match.groups()
                samples.append(
                    {
                        "beat": int(beat),
                        "uptime_s": int(secs) + int(millis) / 1000.0,
                        "tick": int(tick),
                        "wdg_ok": int(wdg_ok),
                        "txdrop": int(txdrop),
                        "rxdrop": int(rxdrop),
                        "covr": int(covr),
                        "dwt": dwt,
                        "rearm": int(rearm),
                        "fsq_failed": int(q_failed),
                        "fsq_rejected": int(q_rejected),
                        "rt": rt,
                        "host_s": time.monotonic() - start_host,
                    }
                )
        if beat_enabled:
            port.write(b"beat off\n")

    # ---- 判据 ----
    usable = samples[WARMUP_SAMPLES:]
    problems: list[str] = []
    if reboots > 1:
        problems.append(f"运行期复位 {reboots - 1} 次（启动横幅出现 {reboots} 次）")
    if faults > 0:
        problems.append(f"故障 {faults} 次")
    txdrop_total = sum(s["txdrop"] for s in usable)
    rxdrop_total = sum(s["rxdrop"] for s in usable)
    if txdrop_total:
        problems.append(f"控制台发送丢弃 {txdrop_total} B")
    if rxdrop_total:
        problems.append(f"控制台接收丢弃 {rxdrop_total} B")
    if any(s["rt"] != "ok" for s in usable):
        problems.append("运行期出现 rt=VIOLATION/none")
    if any(s["dwt"] != "ok" for s in usable):
        problems.append("运行期出现 dwt=DEAD")
    maintenance_overrun_total = sum(s["covr"] for s in usable)
    if any(s["wdg_ok"] == 0 for s in usable):
        problems.append("运行期 wdg_ok=0（健康位未到齐）")
    if any(s["fsq_failed"] or s["fsq_rejected"] for s in usable):
        problems.append("文件系统写队列出现失败/拒绝")

    drift = None
    tick_rate = None
    if len(usable) >= 2:
        first, last = usable[0], usable[-1]
        host_delta = last["host_s"] - first["host_s"]
        device_delta = last["uptime_s"] - first["uptime_s"]
        if host_delta > 0:
            drift = (device_delta - host_delta) / host_delta * 100.0
            if abs(drift) > DRIFT_TOLERANCE_PERCENT:
                problems.append(f"时基漂移 {drift:+.3f}% 超出 ±{DRIFT_TOLERANCE_PERCENT}%")
        tick_delta = last["tick"] - first["tick"]
        if device_delta > 0:
            tick_rate = tick_delta / device_delta
            if abs(tick_rate - 1000.0) > TICK_RATE_TOLERANCE_PERCENT * 10.0:
                problems.append(f"节拍频率 {tick_rate:.1f} Hz 超出 1000±2%")

    summary = {
        "started": stamp,
        "duration_minutes": args.minutes,
        "samples": len(samples),
        "reboots": max(reboots - 1, 0),
        "faults": faults,
        "txdrop_total": txdrop_total,
        "rxdrop_total": rxdrop_total,
        "maintenance_overrun_total": maintenance_overrun_total,
        "drift_percent": drift,
        "tick_rate_hz": tick_rate,
        "problems": problems,
        "raw": raw_path,
    }
    with open(summary_path, "w", encoding="utf-8") as handle:
        json.dump(summary, handle, ensure_ascii=False, indent=2)

    print(f"soak: {len(samples)} 条心跳 · {args.minutes:g} min")
    if drift is not None:
        print(f"  漂移 : {drift:+.3f}%")
    if tick_rate is not None:
        print(f"  节拍 : {tick_rate:.1f} Hz")
    print(f"  txdrop={txdrop_total} rxdrop={rxdrop_total} reboots={max(reboots - 1, 0)} faults={faults}")
    print(f"  摘要 : {summary_path}")
    if problems:
        print("soak 失败：", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print("soak 通过：无复位/无丢弃/无实时违规/漂移在容差内")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
