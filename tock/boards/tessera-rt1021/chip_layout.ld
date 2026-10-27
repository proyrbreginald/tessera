/* Tessera RT1021 的 Tock 布局（Phase 1：容纳 BSP 的段）。
 *
 * - hdr/rom 沿用现有启动头 + bootloader 分区；
 * - prog 指向现有 slot_run（不装 app，仅满足链接符号）；
 * - ram 使用 OCRAM（Tock 内核数据/栈）；
 * - dtcm 给 BSP 的 .dtcm / .uninit 静态量；
 * - romx 放 BSP 的 `.xip` 热路径（bootloader 区后半段，避开内核镜像）。
 */
MEMORY
{
  hdr (rx)   : ORIGIN = 0x60000000, LENGTH = 0x2000
  rom (rx)   : ORIGIN = 0x60002000, LENGTH = 0x3E000
  prog (rx)  : ORIGIN = 0x60040000, LENGTH = 0x200000
  romx (rx)  : ORIGIN = 0x60030000, LENGTH = 0x10000
  ram (rwx)  : ORIGIN = 0x20200000, LENGTH = 0x20000
  dtcm (rw)  : ORIGIN = 0x20000000, LENGTH = 0x10000
}

SECTIONS {
  .boot_hdr : ALIGN(4)
  {
      KEEP (*(.boot_hdr))
  } > hdr

  .xip : ALIGN(4)
  {
      *(.xip)
      *(.xip.*)
  } > romx

  .dtcm (NOLOAD) : ALIGN(4)
  {
      *(.dtcm)
      *(.dtcm.*)
  } > dtcm

  .uninit (NOLOAD) : ALIGN(4)
  {
      *(.uninit)
  } > dtcm

  .fcb : ALIGN(4)
  {
      EXTERN(FLEXSPI_CONFIGURATION_BLOCK);
      KEEP(*(.fcb))
  } > romx

  __sdtcm = ORIGIN(dtcm);
  __edtcm = ORIGIN(dtcm) + LENGTH(dtcm);
}
