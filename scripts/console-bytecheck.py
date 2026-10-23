#!/usr/bin/env python3
"""控制台逐字节校验：复位板子、抓启动输出、逐条比对期望文本。

# 为什么需要它（而不是"看一眼串口有没有字"）

2026-10-10 的教训：控制台发送改成中断驱动后，长输出的**后半段丢了约 40% 的字节**
（3 字节的汉字被整体错位），但**前几行看起来完全正常** —— 肉眼看串口很容易漏掉。
当时的定位手法就是本脚本：把抓包**剥离 ANSI 后逐条比对期望文本**，
并用"换回阻塞式发送"做 A/B 对照，立刻把范围锁进发送实现。

所以这条脚本是**回归工具**：改动控制台/时钟/中断相关代码后跑一次，
"字符是否完整"从此有客观判据，而不是靠人眼。

# 用法

    make bytecheck                 # 用 udev 别名，复位并抓 15 秒
    python3 scripts/console-bytecheck.py --seconds 20 --port /dev/ttyUSB0

退出码 0 = 全部逐字节一致；非 0 = 有缺失/错位，并打印缺失的那一条。
"""
from __future__ import annotations

import argparse
import codecs
import re
import subprocess
import sys
import threading
import time

import os

# 串口别名与 pyocd 的解析顺序**跟随 `scripts/tool-paths.sh`**（那份是唯一事实来源，
# 这里不另写一份默认值；改路径只改那一处）。脚本里用环境变量覆盖。
SERIAL_ALIAS = os.environ.get("UART_ALIAS", "/dev/pudu-debugger-16p-uart")
PYOCD_CANDIDATES = [
    os.environ.get("PYOCD"),
    "./.venv/bin/pyocd",
    # 相邻项目的 .venv（台架工具共享位置；可用 PYOCD 覆盖）。
    "/home/reginald/project/elevator-protocol-gateway/.venv/bin/pyocd",
]


def resolve_pyocd() -> str:
    """按 `tool-paths.sh` 的顺序找 pyocd（找不到就交给 PATH）。"""
    for candidate in PYOCD_CANDIDATES:
        if candidate and os.path.exists(candidate):
            return candidate
    return "pyocd"

# 期望出现的文本（默认固件在启动横幅里确实会打印这些行）。
# 这些字符串**必须按实际渲染写**：终端横幅里的 `help` 是着色的
# （`\x1b[32mhelp\x1b[0m`），所以比对前要先剥 ANSI。
#
# 横幅的**图形部分**不在这里写死：它来自仓库根 `banner.txt`（构建期注入），
# 期望值直接从那个文件取第一行 —— 换图形只改一个文本文件，脚本自动跟随，
# 不会留下"过时的期望字符串"。
def _banner_first_line() -> list:
    path = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "banner.txt"
    )
    try:
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                # 保留行首空格：横幅图形的缩进是画面的一部分，strip 会让期望行
                # 与设备实际输出差一个前导空格（2026-10-21 真机踩到）。
                text = line.rstrip("\n")
                if text.strip():
                    return [text]
    except OSError:
        return []
    return []


# 值级期望：不是"出现这几个字"，而是"这一行的值必须长成什么样"。
# 2026-10-19 真机校准发现旧版只做子串存在性：`sdram post : FAIL`、`can : FAIL`、
# `rt=none` 都能通过，于是"逐字节校验"名不副实。下面每一条都锚定完整行。
#
# 格式：`(说明, 正则)`；正则按 `re.MULTILINE` 搜索。
CHECKS = [
    # 身份行：版本 + 芯片 + 主频。版本由固件从 VERSION 注入，不能只匹配前缀。
    (
        "身份行（版本 + 芯片）",
        r"^tessera \d+\.\d+\.\d+\.\d+ · MIMXRT1021CAG4A · Cortex-M7 @396 MHz$",
    ),
    ("启动节", r"^\[启动\]$"),
    (
        "SDRAM 上电自检",
        r"^  sdram post : \d+ ms · PASS · \d+ KiB · hold \d+/\d+$",
    ),
    ("平台节", r"^\[平台\]$"),
    (
        "4 字节寻址",
        r"^  flash 4B   : PASS · \d+ MiB addressable（>16 MiB 未被别名）$",
    ),
    (
        "SDRAM 器件/时钟",
        r"^  sdram      : W9825G6KH-6I · \d+ MiB @0x[0-9a-f]+ · SEMC \d+ MHz$",
    ),
    (
        "文件系统挂载",
        r"^  fs         : mounted · \d+ × 64 KiB · \d+ files? · .+$",
    ),
    # alloc 必须是 OK；任何 **FAIL** 都会在下面的全局检查里再挡一道。
    (
        "分配池自检",
        r"^  alloc      : OK · 小块 \d+/\d+ · 大块 \d+/\d+ · 拒绝布局 \d+ · 拒绝释放 \d+ · exhaust \d+$",
    ),
    (
        "DMA 池非缓存",
        r"^  dma        : pool 0x20210000\+64 KiB · non-cacheable PASS$",
    ),
    ("安全节", r"^\[安全\]$"),
    (
        "MPU/cache 回读",
        r"^  mpu        : 8 regions verified · I-cache ON · D-cache ON$",
    ),
    (
        "栈余量",
        r"^  stack      : peak \d+ B / 48 KiB · headroom \d+ B \(\d+%\)$",
    ),
    (
        "复位原因",
        r"^  reset      : 0x[0-9a-f]{8} \[.+\] · last crash: .+$",
    ),
    ("实时节", r"^\[实时\]$"),
    # DWT 必须 running；启动期被清后应已自动重装（rearm ≥ 1）。
    (
        "DWT 仪表",
        r"^  dwt        : running · CTRL=0x[0-9a-f]{8} DEMCR=0x[0-9a-f]{8} · rearm \d+$",
    ),
    # flash rd 的值由下面的范围判据单独检查；这里只锚定格式。
    (
        "Flash 读吞吐",
        r"^  flash rd : \d+ KiB/s（\d+ KiB / \d+ µs，checksum=0x[0-9a-f]+，cache ON）$",
    ),
    ("设备节", r"^\[设备\]$"),
    (
        "设备身份",
        r"^  id         : [0-9a-f]{16}（候选，唯一性待手册确认）$",
    ),
    (
        "CAN loopback",
        r"^  can        : alive · PE \d+ Hz · \d+ bps · loopback PASS$",
    ),
    ("终端就绪", r"^纯逻辑终端：输入 `help` 查看命令，Tab 补全，上下键翻历史。$"),
]

# 与"最强底座"的诚实口径一致：XIP 流式读应在几十 MiB/s 量级。
# 低于 10 MiB/s 说明器件/时钟/预取退化；高于 120 MiB/s 说明测到的是缓存命中，
# 不是器件带宽（2026-10-19 实测旧方法给出 3.4M KiB/s，就是这个错）。
FLASH_READ_MIN_KIB_S = 10_000
FLASH_READ_MAX_KIB_S = 120_000
FLASH_READ_PATTERN = re.compile(r"flash rd : (\d+) KiB/s")

ANSI = re.compile(rb"\x1b\[[0-9;]*[A-Za-z]")


def capture(port: str, seconds: float) -> bytes:
    """复位板子并抓一段原始字节（在复位**之前**就开始读，否则会漏掉最早的行）。"""
    try:
        import serial  # type: ignore
    except ImportError:
        print("缺少 pyserial：make setup", file=sys.stderr)
        raise SystemExit(2)

    with serial.Serial(port, 115200, timeout=0.2) as link:
        link.reset_input_buffer()
        threading.Thread(
            target=lambda: subprocess.run(
                [resolve_pyocd(), "reset", "--target", "mimxrt1020"], capture_output=True
            ),
            daemon=True,
        ).start()
        raw = b""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            raw += link.read(65536)
    return raw


def main() -> int:
    parser = argparse.ArgumentParser(description="控制台逐字节校验")
    parser.add_argument("--port", default=SERIAL_ALIAS, help=f"串口节点（默认 {SERIAL_ALIAS}）")
    parser.add_argument("--seconds", type=float, default=15.0, help="抓包时长（秒）")
    args = parser.parse_args()

    port = args.port
    raw = capture(port, args.seconds)
    # 用**增量解码器**：按 chunk 独立 decode 会把跨 chunk 的 UTF-8 汉字截断成 U+FFFD，
    # 看起来像"固件输出乱码"，实际是测量工具的错（我自己踩过一次）。
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    text = decoder.decode(ANSI.sub(b"", raw)) + decoder.decode(b"", final=True)
    # 串口行尾是 ``\r\n``；不归一化的话，锚定行尾的 ``$`` 会匹配不到（2026-10-19 实测）。
    text = text.replace("\r", "")

    print(f"抓到 {len(raw)} 字节（剥离 ANSI 后 {len(text)} 字符）")
    failures: list[str] = []

    # 0) 横幅图形来自仓库根 `banner.txt`（构建期注入）；期望值从同一文件取。
    banner = _banner_first_line()
    if banner:
        ok = re.search(r"^" + re.escape(banner[0]) + r"$", text, re.MULTILINE) is not None
        print(f"  {'✅' if ok else '❌'} 启动横幅（来自 banner.txt）")
        if not ok:
            failures.append("启动横幅首行未出现")

    # 1) 值级/格式级期望：每一条都锚定完整行。
    for description, pattern in CHECKS:
        ok = re.search(pattern, text, re.MULTILINE) is not None
        print(f"  {'✅' if ok else '❌'} {description}")
        if not ok:
            failures.append(f"{description}（期望 /{pattern}/）")

    # 2) 任何 **FAIL** 都是硬失败（启动报告不允许"非预期告警"）。
    if "**FAIL**" in text:
        failures.append("启动报告出现 **FAIL**（不允许的告警）")
        print("  ❌ 启动报告出现 **FAIL**")

    # 3) Flash 读吞吐必须在"器件带宽"的合理范围；超出范围说明测的是缓存命中。
    match = FLASH_READ_PATTERN.search(text)
    if match:
        kib_s = int(match.group(1))
        if FLASH_READ_MIN_KIB_S <= kib_s <= FLASH_READ_MAX_KIB_S:
            print(f"  ✅ Flash 读吞吐 {kib_s} KiB/s 在诚实范围")
        else:
            print(
                f"  ❌ Flash 读吞吐 {kib_s} KiB/s 超出范围 "
                f"[{FLASH_READ_MIN_KIB_S}, {FLASH_READ_MAX_KIB_S}]"
            )
            failures.append(f"Flash 读吞吐 {kib_s} KiB/s 超出诚实范围")

    # 4) 栈余量必须 ≥30%（与 `bench` 的硬下限一致）。
    match = re.search(r"headroom \d+ B \((\d+)%\)", text)
    if match and int(match.group(1)) < 30:
        failures.append(f"栈余量 {match.group(1)}% 低于硬下限 30%")
        print(f"  ❌ 栈余量 {match.group(1)}% 低于硬下限 30%")

    if failures:
        print(f"\n逐字节校验失败：{len(failures)} 项", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("\n逐字节校验通过：全部期望行与值级判据成立")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
