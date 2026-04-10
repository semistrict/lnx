package lnx

// MemorySnapshot describes a Firecracker memory+state snapshot to restore.
type MemorySnapshot struct {
	StatePath string
	MemPath   string
}
