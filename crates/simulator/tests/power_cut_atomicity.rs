//! 掉电原子性：**任意时刻**掉电，读回必须是完整旧值或完整新值。
//!
//! # 为什么这是本项目最有价值的一组测试
//!
//! `Store::write_atomic` 的契约（"要么旧值、要么新值"）**无法用类型表达**。
//! 早期设计因此把它列为"只能靠真机断电测试验证"的薄弱点 —— 而真机断电测试慢、
//! 不可复现、一天跑不了几次。
//!
//! 把策略（`DoubleBankStore`，纯逻辑）与介质（`RawMedium`，不可测）拆开之后，
//! 策略可以在主机上被**穷举**：对每一个可能的掉电字节位置各跑一次，
//! 断言读路径的安全性。
//!
//! `MemMedium` 的掉电模型是**忠实**的：编程中途掉电会留下"已写入的字节 +
//! 未写入（0xFF）的字节"，而不是整体回滚 —— 真实 NOR Flash 正是如此。

use ports::double_bank::{DoubleBankStore, HEADER_LEN, TRAILER_LEN};
use ports::store::{Region, Store, StoreError};
use simulator::MemMedium;

/// 槽位 512 字节；介质扇区 256 字节（要求槽位是扇区整数倍）。
const SLOT: usize = 512;
/// 具体存储类型（避免与 `Store` trait 同名）。
type TestStore = DoubleBankStore<MemMedium<4096>, SLOT>;

const OLD: &[u8] = b"CONFIG-OLD-0001";
const NEW: &[u8] = b"CONFIG-NEW-0002";

fn read_value(store: &mut TestStore) -> Result<Vec<u8>, StoreError> {
    let mut out = [0u8; 64];
    let len = store.read(Region::Config, &mut out)?;
    Ok(out[..len].to_vec())
}

#[test]
fn after_any_power_cut_the_old_value_is_still_readable() {
    let total_written = HEADER_LEN + NEW.len() + TRAILER_LEN;

    for cut in 0..=total_written {
        let mut store = TestStore::new(MemMedium::new());
        store
            .write_atomic(Region::Config, OLD)
            .expect("首次写入应当成功");

        // 再写新值，但在第 `cut` 个字节处掉电。
        store.medium_mut().power_cut_after(cut);
        let _ = store.write_atomic(Region::Config, NEW);

        match read_value(&mut store) {
            Ok(value) => assert!(
                value == OLD || value == NEW,
                "截断点 {cut}：读回了既不是旧值也不是新值的 {} 字节：{:?}",
                value.len(),
                value
            ),
            Err(error) => panic!("截断点 {cut}：旧值必须仍然可读，实际得到 {error:?}"),
        }
    }
}

#[test]
fn first_write_cut_never_yields_a_torn_value() {
    let total_written = HEADER_LEN + NEW.len() + TRAILER_LEN;
    for cut in 0..=total_written {
        let mut store = TestStore::new(MemMedium::new());
        store.medium_mut().power_cut_after(cut);
        let _ = store.write_atomic(Region::Config, NEW);

        match read_value(&mut store) {
            // 只有**完整**写入才允许读出内容。
            Ok(value) => assert_eq!(value, NEW, "截断点 {cut}：读出了不完整的内容"),
            // 未写完即视为"没有可信内容"（上层会用安全默认值）。
            Err(StoreError::Empty | StoreError::Checksum) => {}
            Err(other) => panic!("截断点 {cut}：出现了未预期的错误 {other:?}"),
        }
    }
}

#[test]
fn a_torn_newest_record_does_not_mask_the_intact_older_one() {
    // 这条是设计评审时发现的真实缺陷：早期实现"先挑最新的、再校验它"，
    // 于是一旦最新那份是半写的，读路径会报错并**丢掉仍然完好的旧值**。
    // 正确顺序是"先全部校验，再在有效的那些里挑最新"。
    let mut store = TestStore::new(MemMedium::new());
    store.write_atomic(Region::Config, OLD).unwrap();

    // 只允许写入头部（10 字节）：新记录的序号已落盘、负载未写 → CRC 必然失败。
    store.medium_mut().power_cut_after(HEADER_LEN);
    assert!(store.write_atomic(Region::Config, NEW).is_err());

    assert_eq!(read_value(&mut store).unwrap(), OLD, "旧值必须仍然可用");
}

#[test]
fn repeated_writes_keep_erasing_and_never_lose_the_latest_value() {
    let mut store = TestStore::new(MemMedium::new());
    store.write_atomic(Region::Config, b"v1").unwrap();
    let after_first = store.medium().erase_calls();
    store.write_atomic(Region::Config, b"v2").unwrap();
    store.write_atomic(Region::Config, b"v3").unwrap();
    assert!(
        store.medium().erase_calls() > after_first,
        "后续写入必须发生擦除（否则写进了未擦除区域）"
    );
    assert_eq!(read_value(&mut store).unwrap(), b"v3");
}

#[test]
fn regions_are_independent() {
    let mut store = TestStore::new(MemMedium::new());
    store.write_atomic(Region::Config, b"config").unwrap();
    store.write_atomic(Region::Dataset, b"dataset").unwrap();
    store.write_atomic(Region::BlackBox, b"log").unwrap();

    let mut out = [0u8; 32];
    let n = store.read(Region::Dataset, &mut out).unwrap();
    assert_eq!(&out[..n], b"dataset");
    let n = store.read(Region::Config, &mut out).unwrap();
    assert_eq!(&out[..n], b"config");
    let n = store.read(Region::BlackBox, &mut out).unwrap();
    assert_eq!(&out[..n], b"log");
}

#[test]
fn oversized_payload_is_rejected_without_touching_the_stored_value() {
    let mut store = TestStore::new(MemMedium::new());
    store.write_atomic(Region::Config, OLD).unwrap();
    let too_big = [0u8; SLOT];
    assert_eq!(
        store.write_atomic(Region::Config, &too_big),
        Err(StoreError::TooLarge)
    );
    assert_eq!(read_value(&mut store).unwrap(), OLD);
}

#[test]
fn empty_region_reports_empty_not_corruption() {
    let mut store = TestStore::new(MemMedium::new());
    let mut out = [0u8; 16];
    assert_eq!(store.read(Region::Config, &mut out), Err(StoreError::Empty));
}
