# Tessera 常用指令
#
# 不带目标直接 `make` 会打印这份帮助。
# 说明：Makefile 只做**转发**，真正的验证逻辑在 `cargo xtask`（唯一一份）。

SHELL := /bin/bash
.DEFAULT_GOAL := help

HOST_TARGET ?= x86_64-unknown-linux-gnu
FW_TARGET   ?= thumbv7em-none-eabihf
ELF         := target/$(FW_TARGET)/release/firmware

.PHONY: help
help: ## 显示本帮助
	@printf '\n\033[1mbase\033[0m  （PJ0007_TF2 / MIMXRT1021 / Rust + RTIC v2）\n\n'
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| sort | awk 'BEGIN {FS = ":.*?## "} {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
	@printf '\n首次接手： make setup → make check-bench → make verify\n'
	@printf '文档地图： docs/README.md\n\n'

# ------------------------------------------------------------------ 环境

.PHONY: setup
setup: ## 建 Python 虚拟环境（pyocd/pyserial，无需 root）
	@bash scripts/setup-bench.sh

.PHONY: check-bench
check-bench: ## 台架就绪门禁（烧录前必须通过）
	@bash scripts/check-bench.sh

# ------------------------------------------------------------------ 构建与验证

.PHONY: build
build: ## 编译 release 固件
	cargo build --release

.PHONY: test
test: ## 主机测试（纯逻辑层，不碰硬件）—— 委托 xtask，crate 列表只有一份
	cargo xtask test-host

.PHONY: check
check: ## 静态检查：fmt + clippy（固件目标与主机目标各一次）—— 委托 xtask
	cargo fmt --all --check
	cargo xtask lint-target
	cargo xtask lint-host

.PHONY: gates
gates: ## 全部静态门禁的汇总表（不因第一个失败而中断）
	cargo xtask gates

.PHONY: coverage
coverage: ## 主机侧覆盖率（阈值 90%）
	cargo xtask coverage

.PHONY: context
context: ## 机器可读的项目上下文（JSON，供 Agent/脚本用）
	cargo xtask context

.PHONY: verify
verify: ## 全量离线验证（门禁 + lint + 测试 + 构建 + 产物断言）
	@bash scripts/verify.sh

.PHONY: evidence
evidence: ## 登记一次真机证据（版本/镜像/板卡/抓包）到 tmp/evidence/
	@bash scripts/record-evidence.sh

.PHONY: release-check
release-check: verify ## 发布前检查：verify + 覆盖率 + 台架门禁 + bytecheck + monitor
	cargo xtask coverage
	@bash scripts/check-bench.sh
	@bash scripts/bytecheck.sh
	@.venv/bin/pyocd reset -t mimxrt1020
	@sleep 1
	@bash scripts/monitor.sh

.PHONY: image
image: build ## 对链接后的 ELF 断言 FCB/IVT/向量表布局
	cargo xtask check-image

.PHONY: size
size: build ## 固件体积与上限
	cargo xtask check-size
	@arm-none-eabi-size $(ELF) 2>/dev/null || size $(ELF)

.PHONY: objcopy
objcopy: build ## 生成可交付的 .bin / .hex
	arm-none-eabi-objcopy -O binary $(ELF) firmware.bin
	arm-none-eabi-objcopy -O ihex   $(ELF) firmware.hex
	@ls -l firmware.bin firmware.hex

.PHONY: tree
tree: ## 打印工程结构
	@find . -path ./target -prune -o -path ./.git -prune -o -path ./.venv -prune \
		-o -type f -print | grep -v '^\./tmp' | sort | sed 's|^\./||'

# ------------------------------------------------------------------ 真机

.PHONY: flash
flash: ## 烧录（先跑台架门禁）
	@bash scripts/flash.sh

.PHONY: run
run: flash ## 烧录并抓串口
	@bash scripts/monitor.sh

.PHONY: bytecheck
bytecheck: ## 控制台逐字节校验（复位并比对期望文本；改动控制台/时钟后必跑）
	@bash scripts/bytecheck.sh

.PHONY: monitor
monitor: ## 只看串口并检验时基漂移
	@bash scripts/monitor.sh

SOAK_MINUTES ?= 30
.PHONY: soak
soak: ## 长稳采集（默认 30 分钟；SOAK_MINUTES=... 可调，24–72 h 用于发布前）
	@.venv/bin/python scripts/soak.py --minutes $(SOAK_MINUTES)

.PHONY: test-target
test-target: ## 目标端自检（复位 → selftest → 逐条解析；需要板子）
	@.venv/bin/python scripts/target_test.py

.PHONY: reset
reset: ## 复位目标板
	@./.venv/bin/pyocd reset --target mimxrt1020

# ------------------------------------------------------------------ 维护

.PHONY: fmt
fmt: ## 格式化代码
	cargo fmt --all

.PHONY: trace
trace: ## 生成需求追溯矩阵
	cargo xtask trace
	@echo "→ docs/01-需求/需求追溯矩阵.md"

.PHONY: gaps
gaps: ## 列出未实现项（当前交付的诚实边界）
	cargo xtask check-gaps

.PHONY: clean
clean: ## 清理构建产物
	cargo clean
	rm -f firmware.bin firmware.hex
