/* Tessera RT1021 的最小 Tock 布局：
 * - hdr/rom 沿用现有 boot_header + bootloader 分区（0x6000_0000..0x6004_0000）
 * - prog 指向现有 slot_run（不装 app，仅满足链接符号）
 * - ram 使用 FlexRAM 默认划分的 OCRAM（0x2020_0000，128 KiB）
 */
MEMORY
{
  hdr (rx)   : ORIGIN = 0x60000000, LENGTH = 0x2000
  rom (rx)   : ORIGIN = 0x60002000, LENGTH = 0x3E000
  prog (rx)  : ORIGIN = 0x60040000, LENGTH = 0x200000
  ram (rwx)  : ORIGIN = 0x20200000, LENGTH = 0x20000
}

SECTIONS {
  .boot_hdr : ALIGN(4)
  {
      KEEP (*(.boot_hdr))
  } > hdr
}
