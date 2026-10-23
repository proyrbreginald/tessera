# 微内核与 capsule 架构

> 本文是平台架构的**方向性真源**：底座按“core kernel + 基础 capsule + 业务 capsule”
> 组织，业务以独立 crate 参与编译期组合。
>
> 决策记录见 [ADR-0008](../adr/0008-静态capsule化内核.md)。
> 本文明确区分“**当前已具备**”与“**目标形态 / 尚未实现**”，不把规划写成现状。

## 1. 一句话结论

采用**静态 capsule 化内核**：

- **core kernel** 独占硬件、中断、调度与实时预算；
- **基础 capsule** 提供终端、存储、日志、协议等平台服务；
- **业务 capsule** 是仓库之外的独立 crate，只依赖稳定的 capsule SDK 门面；
- 全部在**编译期静态组合**，运行期零加载、零热替换；
- 用 Rust 类型系统与门禁在编译期验证契约，而不是运行期动态绑定。

这不是 Tock 的完整复制：Tock 的 capsules 仍运行在内核态、与 kernel 共享地址空间，
`forbid(unsafe_code)` 是代码纪律，不是安全隔离边界。本平台采用同一级别的**可信插件**
模型；不可信第三方代码需要 userspace/MPU 进程模型，明确不在本架构范围内。

## 2. 分层

```text
┌──────────────────────────────────────────────────────────────┐
│ 业务 capsule（仓库之外，独立 crate）                          │
│   - 领域类型 / 状态机 / 业务协议语义                          │
│   - 只依赖 capsule SDK 门面；无 unsafe、无寄存器、无 RTIC 任务 │
└───────────────────────▲──────────────────────────────────────┘
                        │ capability 句柄 + 事件
┌───────────────────────┴──────────────────────────────────────┐
│ 基础 capsule / 系统服务（本仓库的纯逻辑层）                    │
│   终端、文件系统、日志、gzip、协议编解码、Flash 纪律……         │
└───────────────────────▲──────────────────────────────────────┘
                        │ 端口契约（Bus/Clock/Store/LogSink）
┌───────────────────────┴──────────────────────────────────────┐
│ core kernel（本仓库的 board-support + firmware）                     │
│   启动链、时钟、中断、MPU/cache、Flash 原语、事件循环、故障处理  │
│   唯一允许 unsafe 与寄存器访问的边界                            │
└──────────────────────────────────────────────────────────────┘
```

### 2.1 各层职责

| 层 | 应当做什么 | 不应当做什么 |
|---|---|---|
| core kernel | 拥有硬件、中断向量、时钟、MPU/cache、内存池、故障处理；向 capsule 分发事件与能力句柄；统一测量实时预算 | 不包含业务语义；不把硬件句柄直接暴露给业务 |
| 基础 capsule | 实现可复用的平台服务；只通过端口访问 kernel 能力；纯逻辑可主机测试 | 不直接操作寄存器；不私自创建 RTIC 任务/中断 |
| 业务 capsule | 实现领域状态机与业务决策；用 `simulator` 主机重放 | 不依赖 `board-support`/`imxrt-*`/`cortex-m*`/`rtic`；不阻塞、不 panic 作为控制流 |

## 3. capsule 契约（S1 已实现的部分 + 目标形态）

> 当前仓库**已有** `sdk` 门面（`Capsule<C>` / `CapsuleSet` / `Product`）与产品组合根
> `product`（S1）。**尚未实现**的是 S2 上下文能力对象化（`CapsuleContext` / 能力句柄）
> 与 S3 基础 capsule 外提。设计见 [`产品组合根外提设计`](产品组合根外提设计.md)。

### 3.1 生命周期（实际 API）

capsule 是被动状态机，由组合根在固定上下文调用。`crates/sdk/src/lib.rs` 的实际契约：

```rust
pub trait Capsule<C> {
    fn name(&self) -> &'static str;
    fn phase(&self) -> CapsulePhase;                  // 默认 Maintenance
    fn init(&mut self) -> Result<(), CapsuleError>;   // 失败即标记不可用
    fn step(&mut self, ctx: &mut C) -> CapsuleStep;   // Idle/Worked/Failed/Deferred
}
```

- `C` 是 step 上下文，对组合根是**不透明泛型参数**；当前 `firmware` 传入的
  `TerminalStepContext` 是 `pub(crate)`，所以仓库外 capsule 只能写 `impl<C> Capsule<C>`（无能力）；
- 阶段顺序由 `CapsulePhase`（Input → Maintenance → Health）决定，不由注册顺序决定；
- **事件分发 `on_event` 未实现**：当前只有 `init`/`step` 两个入口，没有 kernel 事件队列；

`CapsuleSet<'a, C, N>` 与 `Product<'a, C, N>` 提供固定容量静态组合：注册（容量满/重名显式失败）、
`init_all`（失败后跳过该 capsule 的后续 step）、按阶段 `step`、**累计**失败 3 次即隔离（`step_failures` 成功不清零，`sdk` 的 `MAX_STEP_FAILURES`），以及
`ProductStatus` 快照。

约束：

- `init` 只做初始化，不得启动长活；
- `step` 由组合根在空闲循环里调用，必须快速返回并报告本轮是否做了工作；
- 不允许 capsule 自己注册中断或创建 RTIC 任务。

### 3.2 编译期组合

组合根在编译期决定 capsule 集合与容量：`Product<'a, C, N>` 的 `N` 是编译期常量，
`CapsuleSet` 只是 `[Option<Slot>; N]` 固定数组。默认产品（`product`）的容量是
`BASE_CAPSULES = 6` + `BUSINESS_SLOTS = 2` = `CAPSULE_SLOTS = 8`；服务实例通过
`product::Platform` 注入，组合根不认识任何具体服务类型。

要求：

- 不用运行期注册表、不用 `linkme`、不用堆分配；
- 组合根是唯一知道“产品包含哪些 capsule、什么顺序、多大容量”的地方；
- “业务 crate 不修改底座源码即可参与组合”是 **S3 目标**：S1 只外提组合策略
  （集合/顺序/容量/产品名）。`Product` 借用 capsule，业务实例必须由调用方持有并活过
  组合根；仓库外产品 crate 不能在自己的 `build` 里构造局部实例再返回（生命周期不足）；
- `cargo xtask check-capsules` 约束仓库内插件不直接依赖硬件/组合根。

### 3.3 capability 句柄（S2 目标形态，**未实现**）

> 当前 `sdk` 不提供下列句柄层：`prelude` 只重导出 5 个 `ports` trait，
> 仓库外 capsule 拿不到可调用的能力。本节是 S2 的冻结方向。

业务 capsule 不拿全局静态资源，只拿到组合根授予的句柄：

| 句柄 | 能力 | 对应现有端口 |
|---|---|---|
| `BusHandle` | 发送/接收一帧 | `ports::bus::Bus` |
| `ClockHandle` | 读时间、算截止时刻 | `ports::clock::Clock` |
| `StoreHandle` | 原子写/读持久化数据 | `ports::store::Store` |
| `LogHandle` | 写日志事件 | `ports::log::LogSink` |
| `RawMediumHandle` | 仅诊断 capsule 可用的介质原语 | `ports::raw::RawMedium` |

句柄的粒度和授权规则由组合根决定；业务 capsule 不能凭空构造句柄。

### 3.4 事件与调度

- 中断由 core kernel 独占；ISR 只做有界搬运/置位，事件进入有界队列；
- capsule 的 `on_event` / `step` 在可打断上下文执行；
- 每个 capsule 的 `step` 用 `realtime` 独立计量，超预算必须可见；
- 硬实时业务如果需要固定周期，由 kernel 提供固定优先级钩子，而不是让业务自建任务。

## 4. 与当前仓库的映射

### 4.1 当前已具备

| 能力 | 当前实现 |
|---|---|
| 硬件/中断/启动归 core kernel | [`board-support`](../../crates/board-support/src/lib.rs) + [`firmware`](../../crates/firmware/src/main.rs) |
| 端口契约 | [`ports`](../../crates/ports/src/lib.rs)：`Bus`/`Clock`/`Store`/`LogSink`/`RawMedium` |
| 主机替身 | [`simulator`](../../crates/simulator/src/lib.rs)：脚本总线/手动时钟/掉电介质/捕获日志 |
| 纯逻辑可测 | 18 个纯逻辑 crate；`check-layering` 强制无硬件依赖 |
| capsule SDK | [`sdk`](../../crates/sdk/src/lib.rs)：`Capsule<C>` / `CapsuleSet` / `Product` / `prelude` |
| 产品组合根 | [`product`](../../crates/product/src/lib.rs)：`Platform` 注入 + `build`，只依赖 `sdk` |
| 插件门禁 | `check-capsules`：`sdk` 纯逻辑、仓库内插件不直接依赖硬件/组合根 |
| 业务在仓库外 | [`业务接入指南`](业务接入指南.md) 已规定业务 crate 的依赖与测试边界 |
| 运行时隔离边界 | `unsafe` 只在 `board-support`/`firmware`，有预算与 `SAFETY` 配平 |

### 4.2 状态逐项（不得把目标当现状）

| 目标能力 | 当前状态 |
|---|---|
| `sdk` 稳定门面 | **已实现**：`Capsule<C>`/`CapsuleSet`/`Product`/`prelude`；`on_event` 未实现 |
| `Capsule` trait | **已实现**（name/phase/init/step）；事件分发 `on_event` **未实现** |
| 编译期 capsule 组合列表 | **已实现**：`Product<'a, C, N>` 的固定容量 `CapsuleSet` |
| 产品组合根外提 | **已实现（S1）**：`product` crate + `firmware::platform` 注入；旧 `composition.rs` 已删除。S1 只外提组合策略，业务 capsule 的所有权仍在调用方（仓库外接入属 S3） |
| capability 句柄 / step 上下文能力 | **未实现（S2）**：`C` 仍是不透明泛型，仓库外 capsule 拿不到时钟/总线句柄 |
| 基础 capsule 物理外提 | **未实现（S3）**：六个服务仍在 `firmware`，要经 `board-support` 碰硬件 |
| capsule 级预算/健康/配额 | **部分**：有 per-capsule 失败计数与**累计** 3 次即隔离，但没有独立实时预算/配额 |
| 插件门禁与模板 | **部分**：`check-capsules` 已存在，但**只扫仓库内**；仓库外门禁与模板待 S3 |
| 运行期热替换 | **明确不做** |
| 不可信第三方隔离 | **明确不做**，需要 userspace/MPU 进程模型 |

## 5. 安全与信任边界

必须把三件事分开：

1. **内存安全**：safe Rust + `forbid(unsafe_code)` 降低 UB 风险；
2. **接口契约**：类型系统保证“调用顺序/参数/所有权”在编译期成立；
3. **安全隔离**：需要 MPU/进程/特权级，本架构不提供。

因此：

- 业务 capsule 必须是**可信代码**；
- 一个 capsule 的 panic、死循环、资源耗尽仍可能影响整机；
- 防护手段是：每 capsule 预算/健康、看门狗（P5）、资源配额、代码评审与门禁，
  而不是“safe 所以安全”。

## 6. 更新语义

“无缝更新”在本架构中的真实含义：

- 底座与业务在**源码/编译层面解耦**；
- 业务作为独立 crate 参与整包固件编译；
- 接口不兼容在编译期失败，而不是运行期加载失败；
- **不支持运行期热替换**，也没有必要为它引入 ELF 加载器/堆/符号解析。

现场整包更新仍需要 A/B 槽与 Bootloader（P4）。当前 Bootloader 未实现，
所以“更新”暂时仍是通过调试器烧录；这不影响 capsule 化本身。

## 7. 落地顺序（S1 已落地，后续未实现）

1. ✅ **冻结 SDK 门面**（S1）：业务只依赖一个稳定 crate，而不是内部 crate 集合；
2. ✅ **定义 capsule 契约**（S1）：生命周期 name/phase/init/step 与固定容量组合；
3. ✅ **外提产品组合根**（S1）：`product` crate + `Platform` 注入，只外提组合策略；
   “业务不改底座源码即可组合”的所有权通路属 S3；
4. ⬜ **S2：上下文能力对象化**：`CapsuleContext`/能力句柄，`Capsule` 去泛型化；
5. ⬜ **S3：基础 capsule 外提 + 插件模板**：把仓库外业务 crate 纳入同等质量约束；
6. ⬜ **补 capsule 级配额/健康**；
7. ⬜ **A/B 整包更新（P4）**：等明确要求再推进。

每一步都必须先有可失败的判据，再落地实现；在实现前保持“未实现”的诚实口径。
分阶段判据见 [`产品组合根外提设计`](产品组合根外提设计.md) §5。

## 8. 与其他文档的关系

- 依赖方向与 unsafe 边界：[`分层与依赖规则`](分层与依赖规则.md)
- 业务怎么写、怎么主机测试：[`业务接入指南`](业务接入指南.md)
- 为什么这样分层、一次数据流：[`架构总览`](架构总览.md)
- 测试证据链：[`测试策略`](../04-开发/测试策略.md)
- 安全边界与危害分析：[`安全计划`](../05-安全/安全计划.md)
