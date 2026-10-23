//! 固件版本号。
//!
//! # 来源与规则
//!
//! `VERSION` 文件（仓库根）是**唯一真源**，由 `build.rs` 读入并注入环境变量。
//! 规则：**凡是改动固件行为，必须递增版本号**，因为它是整包升级的门禁之一
//! （升级受理要求设备上报的版本与发布记录匹配）。
//!
//! 只改文档/测试工具不必递增；改代码必须递增。
//!
//! # 为什么版本号要参与编译期
//!
//! 若版本号只是运行期常量，就会出现"代码改了但忘了改版本"的静默漂移 ——
//! 而设备会带着错误的版本号去申请升级，表现为"升级被拒绝且原因不明"。
//! 现在版本号的**格式**在构建期校验（见 `build.rs`），数值变化也会改变镜像内容，
//! 因此"改代码同时改版本"这条规则至少留下了构建产物层面的痕迹。

/// 完整版本字符串，例如 `0.21.0.0`。
pub(crate) const VERSION: &str = env!("BASE_VERSION");

/// 把 `build.rs` 注入的十进制文本转成 `u16`（测试用：核对 VERSION 可解析）。
///
/// `build.rs` 已经保证格式合法，因此这里的回退值取 0 只是为了让 `const` 上下文
/// 无需 `unwrap`（本 crate 禁止 unwrap）。真出现非法值，构建期就已经失败了。
// 显式放行下标 lint：循环条件 `index < bytes.len()` 保证了越界不成立，
// 而 `const fn` 里不能用 `get()`（`Option::ok_or` 不是常量特性）。
#[cfg(test)]
#[allow(clippy::indexing_slicing)]
const fn parse_u16(text: &str) -> u16 {
    let bytes = text.as_bytes();
    let mut value: u16 = 0;
    let mut index = 0;
    while index < bytes.len() {
        let digit = match bytes[index] {
            b'0'..=b'9' => bytes[index] - b'0',
            _ => return 0,
        };
        // `digit` 已限定在 0..=9，u8→u16 是**加宽**转换、不可能截断；
        // 这里用 `as` 是因为 `u16::from` 在 `const fn` 里还不是常量特性。
        value = value.wrapping_mul(10).wrapping_add(digit as u16);
        index += 1;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_round_trips_through_build_script() {
        let first = VERSION.split('.').next().unwrap_or("0");
        assert!(parse_u16(first) > 0, "版本号必须由 VERSION 文件提供");
    }

    #[test]
    fn parser_handles_edges() {
        assert_eq!(parse_u16("0"), 0);
        assert_eq!(parse_u16("65535"), 65535);
        assert_eq!(parse_u16(""), 0);
        assert_eq!(parse_u16("12a"), 0, "非法输入必须回退到 0 而不是 panic");
        // 溢出回绕（构建期已保证不会发生，这里固定行为）
        assert_eq!(parse_u16("99999"), 99999u32.wrapping_sub(65536) as u16);
    }
}
