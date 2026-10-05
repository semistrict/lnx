// vring.c — Split virtqueue operations based on QEMU hw/virtio/virtio.c.
//
// Standalone implementation with no QEMU dependencies. Operates on raw
// guest memory pointers. All memory barriers match QEMU's semantics for
// ARM64.
//
// Layout (virtio spec 1.x, split virtqueue):
//
//   Descriptor table (16 bytes each):
//     uint64_t addr; uint32_t len; uint16_t flags; uint16_t next;
//
//   Available ring (driver → device):
//     uint16_t flags; uint16_t idx; uint16_t ring[num]; uint16_t used_event;
//
//   Used ring (device → driver):
//     uint16_t flags; uint16_t idx; struct { uint32_t id; uint32_t len; } ring[num];

#include <stdint.h>
#include <string.h>

// ARM64 memory barriers matching QEMU's smp_wmb/smp_rmb/smp_mb.
#if defined(__aarch64__)
static inline void smp_rmb(void) { __asm__ volatile("dmb ishld" ::: "memory"); }
static inline void smp_wmb(void) { __asm__ volatile("dmb ishst" ::: "memory"); }
static inline void smp_mb(void)  { __asm__ volatile("dmb ish"   ::: "memory"); }
#else
// x86 has strong ordering; compiler barrier suffices for rmb/wmb.
static inline void smp_rmb(void) { __asm__ volatile("" ::: "memory"); }
static inline void smp_wmb(void) { __asm__ volatile("" ::: "memory"); }
static inline void smp_mb(void)  { __asm__ volatile("" ::: "memory"); }
#endif

// Descriptor flags.
#define VRING_DESC_F_NEXT  1
#define VRING_DESC_F_WRITE 2

// vring_desc holds a parsed descriptor.
struct vring_desc {
    uint64_t addr;
    uint32_t len;
    uint16_t flags;
    uint16_t next;
};

// Read a descriptor from the descriptor table at index idx.
static struct vring_desc read_desc(uint8_t *desc_table, uint16_t idx) {
    uint8_t *p = desc_table + (uint32_t)idx * 16;
    struct vring_desc d;
    memcpy(&d.addr,  p + 0, 8);
    memcpy(&d.len,   p + 8, 4);
    memcpy(&d.flags, p + 12, 2);
    memcpy(&d.next,  p + 14, 2);
    return d;
}

// Read the avail ring index (avail->idx at offset 2).
static uint16_t read_avail_idx(uint8_t *avail) {
    uint16_t v;
    memcpy(&v, avail + 2, 2);
    return v;
}

// Read avail ring entry at position pos (avail->ring[pos] at offset 4 + pos*2).
static uint16_t read_avail_ring(uint8_t *avail, uint16_t pos, uint32_t num) {
    uint16_t v;
    uint32_t off = 4 + (uint32_t)(pos % num) * 2;
    memcpy(&v, avail + off, 2);
    return v;
}

// --- Exported to Go via CGO ---

// vring_has_avail returns 1 if the available ring has unprocessed entries.
int vring_has_avail(uint8_t *avail, uint32_t num, uint16_t last_avail_idx) {
    return last_avail_idx != read_avail_idx(avail);
}

// vring_avail_idx returns the current available ring index.
uint16_t vring_get_avail_idx(uint8_t *avail) {
    return read_avail_idx(avail);
}

// vring_pop_avail pops the next available descriptor head. Returns the
// descriptor head index and advances *last_avail_idx. The caller MUST
// have verified vring_has_avail() first.
//
// Includes smp_rmb() between reading avail idx and reading the ring
// entry / descriptors — critical on ARM64 to prevent stale descriptor
// reads. This matches QEMU's barrier in virtqueue_split_pop().
uint16_t vring_pop_avail(uint8_t *avail, uint32_t num, uint16_t *last_avail_idx) {
    uint16_t lai = *last_avail_idx;

    // Barrier: ensure the avail idx load (done by caller in has_avail)
    // completes before we read the ring entry. Without this, the CPU
    // may speculatively read a stale ring[lai] or stale descriptors.
    smp_rmb();

    uint16_t head = read_avail_ring(avail, lai, num);
    *last_avail_idx = lai + 1;
    return head;
}

// vring_put_used writes an entry to the used ring and increments the
// used index. Matches QEMU's virtqueue_split_fill + virtqueue_split_flush.
//
// Barrier ordering:
//   1. Write used ring entry (id + len)
//   2. smp_wmb() — ensure entry visible before index update
//   3. Write used ring index
//   4. smp_mb() — ensure index visible before IRQ
void vring_put_used(uint8_t *used, uint32_t num, uint16_t desc_idx, uint32_t written) {
    // Read current used index.
    uint16_t used_idx;
    memcpy(&used_idx, used + 2, 2);

    // Write entry at used->ring[used_idx % num].
    uint32_t entry_off = 4 + (uint32_t)(used_idx % num) * 8;
    uint32_t id32 = (uint32_t)desc_idx;
    memcpy(used + entry_off + 0, &id32, 4);
    memcpy(used + entry_off + 4, &written, 4);

    // smp_wmb: entry stores must complete before idx update.
    smp_wmb();

    // Increment used index.
    uint16_t new_idx = used_idx + 1;
    memcpy(used + 2, &new_idx, 2);

    // smp_mb: idx update must be visible before we signal IRQ.
    smp_mb();
}

// vring_read_desc reads descriptor at index idx from the table.
// Returns the descriptor fields via out-parameters.
void vring_read_desc(uint8_t *desc_table, uint16_t idx,
                     uint64_t *addr, uint32_t *len,
                     uint16_t *flags, uint16_t *next) {
    struct vring_desc d = read_desc(desc_table, idx);
    *addr = d.addr;
    *len = d.len;
    *flags = d.flags;
    *next = d.next;
}

// vring_walk_write_chain walks the descriptor chain starting at head,
// copying data into WRITE-flagged descriptors. Returns total bytes written.
//
// This is the device-to-guest path (RX). Only VRING_DESC_F_WRITE
// descriptors are written to.
uint32_t vring_walk_write_chain(
    uint8_t *desc_table, uint32_t num,
    uint8_t *mem, uint64_t ram_base,
    uint16_t head, uint8_t *data, uint32_t data_len)
{
    uint32_t written = 0;
    uint16_t idx = head;

    for (uint32_t i = 0; i < num; i++) {
        struct vring_desc d = read_desc(desc_table, idx);

        if (d.flags & VRING_DESC_F_WRITE) {
            // Translate guest physical address to host virtual.
            uint64_t offset = d.addr - ram_base;
            uint8_t *buf = mem + offset;

            uint32_t remaining = data_len - written;
            uint32_t n = remaining < d.len ? remaining : d.len;
            if (n > 0) {
                memcpy(buf, data + written, n);
                written += n;
            }
        }

        if (!(d.flags & VRING_DESC_F_NEXT))
            break;
        idx = d.next;
    }

    return written;
}
