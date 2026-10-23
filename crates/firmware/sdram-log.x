/* 文本日志环：SDRAM NOLOAD 固定区（不参与启动拷贝/清零）。 */
MEMORY
{
  SDRAM (RWX) : ORIGIN = 0x80000000, LENGTH = 0x2000000
}

SECTIONS
{
  .sdram_stack ORIGIN(SDRAM) + 0x1D40000 (NOLOAD) : ALIGN(8)
  {
    __sdram_stack_start = .;
    KEEP(*(.sdram_stack))
    __sdram_stack_end = .;
  } > SDRAM

  .sdram_gzip ORIGIN(SDRAM) + 0x1D00000 (NOLOAD) : ALIGN(8)
  {
    __sdram_gzip_start = .;
    KEEP(*(.sdram_gzip))
    __sdram_gzip_end = .;
  } > SDRAM

  .sdram_log ORIGIN(SDRAM) + 0x1F00000 (NOLOAD) : ALIGN(8)
  {
    __sdram_log_start = .;
    KEEP(*(.sdram_log))
    __sdram_log_end = .;
  } > SDRAM
}

/* DTCM 尾部：向量表之后给高频小对象，NOLOAD + 启动时清零。 */
SECTIONS
{
  .dtcm (NOLOAD) : ALIGN(32)
  {
    __sdtcm = .;
    KEEP(*(.dtcm .dtcm.*))
    __edtcm = .;
  } > DTCM
}
