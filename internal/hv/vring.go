//go:build darwin

package hv

// #include <stdint.h>
// int vring_has_avail(uint8_t *avail, uint32_t num, uint16_t last_avail_idx);
// uint16_t vring_get_avail_idx(uint8_t *avail);
// uint16_t vring_pop_avail(uint8_t *avail, uint32_t num, uint16_t *last_avail_idx);
// void vring_put_used(uint8_t *used, uint32_t num, uint16_t desc_idx, uint32_t written);
// void vring_read_desc(uint8_t *desc_table, uint16_t idx,
//                      uint64_t *addr, uint32_t *len,
//                      uint16_t *flags, uint16_t *next);
// uint32_t vring_walk_write_chain(
//     uint8_t *desc_table, uint32_t num,
//     uint8_t *mem, uint64_t ram_base,
//     uint16_t head, uint8_t *data, uint32_t data_len);
import "C"
import "unsafe"

// cHasAvail checks for available entries using the C vring implementation.
func (q *Virtqueue) cHasAvail(mem []byte, ramBase uint64) bool {
	if !q.ready {
		return false
	}
	avail := gpa2hva(mem, ramBase, q.driverAddr)
	if avail == nil {
		return false
	}
	return C.vring_has_avail((*C.uint8_t)(unsafe.Pointer(&avail[0])),
		C.uint32_t(q.num), C.uint16_t(q.lastAvailIdx)) != 0
}

// cAvailIdx returns the current available ring index.
func (q *Virtqueue) cAvailIdx(mem []byte, ramBase uint64) uint16 {
	avail := gpa2hva(mem, ramBase, q.driverAddr)
	if avail == nil {
		return 0
	}
	return uint16(C.vring_get_avail_idx((*C.uint8_t)(unsafe.Pointer(&avail[0]))))
}

// cPopAvail pops the next available descriptor head. Includes smp_rmb.
func (q *Virtqueue) cPopAvail(mem []byte, ramBase uint64) uint16 {
	avail := gpa2hva(mem, ramBase, q.driverAddr)
	if avail == nil {
		return 0
	}
	lai := C.uint16_t(q.lastAvailIdx)
	head := C.vring_pop_avail((*C.uint8_t)(unsafe.Pointer(&avail[0])),
		C.uint32_t(q.num), &lai)
	q.lastAvailIdx = uint16(lai)
	return uint16(head)
}

// cPutUsed writes to the used ring with proper barriers.
func (q *Virtqueue) cPutUsed(mem []byte, ramBase uint64, descIdx uint16, written uint32) {
	used := gpa2hva(mem, ramBase, q.deviceAddr)
	if used == nil {
		return
	}
	C.vring_put_used((*C.uint8_t)(unsafe.Pointer(&used[0])),
		C.uint32_t(q.num), C.uint16_t(descIdx), C.uint32_t(written))
}

// cWalkWriteChain writes data into WRITE-flagged descriptors in the chain.
// Returns total bytes written. Used for device-to-guest (RX) path.
func (q *Virtqueue) cWalkWriteChain(mem []byte, ramBase uint64, head uint16, data []byte) uint32 {
	descTable := gpa2hva(mem, ramBase, q.descAddr)
	if descTable == nil {
		return 0
	}
	var dataPtr *C.uint8_t
	if len(data) > 0 {
		dataPtr = (*C.uint8_t)(unsafe.Pointer(&data[0]))
	}
	return uint32(C.vring_walk_write_chain(
		(*C.uint8_t)(unsafe.Pointer(&descTable[0])), C.uint32_t(q.num),
		(*C.uint8_t)(unsafe.Pointer(&mem[0])), C.uint64_t(ramBase),
		C.uint16_t(head), dataPtr, C.uint32_t(len(data))))
}
