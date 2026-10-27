# runtime/ —— 备选运行时（当前：Tock）

> 这里存放**不参与生产固件构建**的运行时实验。生产固件仍在 `crates/`（RTIC，`cargo xtask verify`）。
> 分类原则：`crates/` 是 Tessera 自己维护的生产代码；`runtime/` 是「换运行时」的
> vendored 上游源码 + 最小本地板级 crate；`docs/` 记录结论与前置条件。

```text
runtime/
└── tock/
    ├── README.md          # 上游版本、裁剪清单、本地补丁、构建/验证命令
    ├── Cargo.toml         # 裁剪后的 Tock workspace（不是 Tessera workspace 成员）
    ├── kernel/ arch/ chips/ libraries/   # vendored 上游 Tock 2.3.0（仅保留用到的部分）
    ├── boards/
    │   ├── build_scripts/ # 上游构建脚本
    │   └── tessera-rt1021/# ★ 本仓库唯一本地维护的板级 crate
    └── patches/           # 对 vendored 上游的 2 个本地补丁（审计/升级用）
```

- 构建/烧录/验证统一走 `cargo xtask`（Rust，不依赖 make）：
  `cargo xtask tock-build` / `cargo xtask tock-flash` / `cargo xtask tock-verify`
- 设计记录：[`docs/02-架构/Tock替换RTIC验证.md`](../docs/02-架构/Tock替换RTIC验证.md)
- 模板/细节：[`runtime/tock/boards/tessera-rt1021/README.md`](tock/boards/tessera-rt1021/README.md)
