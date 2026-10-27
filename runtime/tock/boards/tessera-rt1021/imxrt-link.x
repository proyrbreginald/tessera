/* TESSERA: 空占位链接脚本。
 *
 * 为什么需要这个文件：
 * 本板源码树位于 /home/reginald/project/tessera 之下，cargo 会把上层
 * /home/reginald/project/tessera/.cargo/config.toml 也合并进来；该文件给
 * `[target.thumbv7em-none-eabihf]` 设了 `-C link-arg=-Timxrt-link.x`
 * （给 RTIC 固件用的脚本，由 imxrt-rt 的 build.rs 生成，这里并不存在）。
 * cargo 对 rustflags 数组是**跨配置文件合并**的，本板的 .cargo/config.toml
 * 无法把这一项去掉，于是链接命令行上会多一个
 * `-Timxrt-link.x`。
 *
 * 真正的内存布局与段定义全部在 `layout.ld`（它 INCLUDE 了
 * `tock_kernel_layout.ld`），本文件只是一个空的、合法的链接脚本，用来
 * 让那条多出来的 `-T` 解析成功而不产生任何段。
 */
