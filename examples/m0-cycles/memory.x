/* QEMU's mps2-an385 (a Cortex-M3, which runs ARMv6-M code unchanged). */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 4M
  RAM   : ORIGIN = 0x20000000, LENGTH = 4M
}
