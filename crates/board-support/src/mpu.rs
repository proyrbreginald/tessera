//! MPU 区域配置与数据/指令缓存使能 —— **薄适配层**。
//!
//! # 这一层只做三件事
//!
//! 1. 把 [`mpu_regions::REGIONS`] 里的区域写进核内 MPU 寄存器；
//! 2. 打开 I-cache / D-cache；
//! 3. **回读**硬件寄存器并把实际属性打印出来。
//!
//! 区域表与 `RBAR`/`RASR` 的编解码**不在这个 crate 里**，而在纯逻辑 crate
//! [`mpu_regions`]：那里有 33 条主机测试，包括一条专门钉住当年真机 bug 的回归测试
//! （`RBAR` 的**区域号在 `bits[3:0]`、`bit[4]` 才是 VALID**；第一版写成 `index << 4`，
//! 结果 8 次写全落进 region0，只有靠横幅回读才发现）。
//!
//! **这类错误应当在 `cargo test` 里失败，而不是在目标现场失败。**
//! 这也是本层回读代码改调 `mpu_regions::rasr_size` 等解码函数的原因：
//! 回读代码本身也要被测试覆盖，否则"报事实"的那一行自己就成了假事实。
//!
//! # 为什么必须"先 MPU、后 cache"
//!
//! D-cache 一旦打开，**内存属性由 MPU 决定**；MPU 未配置时按 ARMv7-M 默认内存映射，
//! 那会把大片地址当成"可缓存"。所以顺序是：先建好区域属性（尤其是把外设区、
//! 把 Flash 标成**只读**），再开缓存。
//!
//! # 区域表从哪来
//!
//! 取自**本板既有固件的已知正确配置**
//! （`winshare/test/master-control-board-a-06/bsp/board/board.c` 的 `BOARD_ConfigMPU`），
//! 唯一改动是 Flash 区域按实装器件改成 32 MiB。详见 `mpu_regions` 的模块文档。
//!
//! ⚠️ Flash 写入（FlexSPI 擦除/页编程）之后必须**失效**对应范围的
//! D-cache 行，否则读回的是旧缓存 —— 见 `crate::cache`（写入路径已实现，
//! 这条约束由 `store` 的写路径逐块执行）。

use mpu_regions::{
    COUNT, REGIONS, rasr, rasr_access, rasr_size, rasr_tex_shareable_cacheable_bufferable, rbar,
};

/// 配置 MPU 并打开 I-cache / D-cache。
///
/// 必须在**任何**大数据量访问之前调用一次；由固件的 `init` 在 `board::init()` 之前调用。
pub fn init() {
    // SAFETY: 这是上电初始化阶段（RTIC 的 `init` 在中断未使能时执行一次），
    // 且 MPU/SCB/CPUID 都是**核内**寄存器，没有别名使用者。写入顺序遵循
    // ARMv7-M 的要求：先设 `RBAR`（带 VALID），再设 `RASR`；`Peripherals::steal()`
    // 取到的包装类型提供了缓存维护所需的序列化操作。
    unsafe {
        let mut core = cortex_m::Peripherals::steal();
        let mpu = &mut core.MPU;
        let scb = &mut core.SCB;
        let cpuid = &mut core.CPUID;

        // 配置期间先关掉 MPU（改区域时保持关闭，避免半成品属性生效）。
        mpu.ctrl.write(0);

        for (index, region) in REGIONS.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(0);
            mpu.rbar.write(rbar(index, region.base));
            mpu.rasr.write(rasr(region));
        }

        // 使能 MPU：PRIVDEFENA = 1（特权访问对未覆盖区域沿用默认映射，
        // 这样外设区（0x4000_0000 段）按默认内存映射仍是"外设、不可缓存"）。
        mpu.ctrl.write((1 << 2) | 1);

        scb.enable_icache();
        // `enable_dcache` 自己会先失效缓存，并在 D-cache 已使能时直接返回。
        scb.enable_dcache(cpuid);
    }
}

/// 把 MPU/cache 的摘要写成一行（读的是**硬件寄存器**，不是我们期望的表）。
pub fn describe(out: &mut impl core::fmt::Write) {
    // SAFETY: 只读核内寄存器。
    let (icache, dcache, enabled) = state();
    let _ = core::fmt::Write::write_fmt(
        out,
        format_args!(
            "MPU {} · I-cache {} · D-cache {} · {} 个区域",
            if enabled { "已启用" } else { "未启用" },
            on_off(icache),
            on_off(dcache),
            COUNT
        ),
    );
}

/// 区域个数（调用方逐行打印，每行用独立缓冲 —— 一次性格式化 9 行会超出定长缓冲）。
#[must_use]
pub fn region_count() -> usize {
    COUNT
}

/// 打印第 `index` 个区域**硬件里实际生效**的属性。
///
/// 解码全部走 [`mpu_regions`] 里被测试覆盖的函数：回读代码本身也是被验证的对象。
pub fn describe_region(index: usize, out: &mut impl core::fmt::Write) {
    let Some(region) = REGIONS.get(index) else {
        return;
    };
    // SAFETY: 只读核内寄存器；写 RNR 只是为了选中区域供回读。
    let rasr_value = unsafe {
        let mpu = &*cortex_m::peripheral::MPU::PTR;
        mpu.rnr.write(u32::try_from(index).unwrap_or(0));
        cortex_m::asm::dsb();
        mpu.rasr.read()
    };
    let (tex, shareable, cacheable, bufferable) =
        rasr_tex_shareable_cacheable_bufferable(rasr_value);
    let _ = core::fmt::Write::write_fmt(
        out,
        format_args!(
            "region{index}: {:#010x} {:>4} KiB  AP={} TEX={} S={} C={} B={}  {}",
            region.base,
            rasr_size(rasr_value) / 1024,
            rasr_access(rasr_value),
            tex,
            u8::from(shareable),
            u8::from(cacheable),
            u8::from(bufferable),
            region.name
        ),
    );
}

/// `true` → `"ON"`。
fn on_off(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}

/// 当前 cache 与 MPU 的实际状态（供启动横幅打印）。
///
/// 返回 `(指令缓存, 数据缓存, MPU)` 三元组，每项为"是否已使能"。
#[must_use]
pub fn state() -> (bool, bool, bool) {
    // SAFETY: 只读核内寄存器；调用方在任何时刻调用都不会改变系统状态。
    unsafe {
        let core = cortex_m::Peripherals::steal();
        let ccr = core.SCB.ccr.read();
        let mpu_ctrl = core.MPU.ctrl.read();
        (
            ccr & (1 << 17) != 0,
            ccr & (1 << 16) != 0,
            mpu_ctrl & 1 != 0,
        )
    }
}
