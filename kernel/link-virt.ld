/* QEMU virt: RAM starts at 0x8000_0000. The kernel owns the first 1 MiB; the
 * app pool (see platform/virt.rs) follows it. */
MEMORY
{
    RAM : ORIGIN = 0x80000000, LENGTH = 1M
}

INCLUDE kernel-common.ld
