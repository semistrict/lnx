package main

import "github.com/semistrict/lnx"

func memorySnapshotEnabled() bool {
	return lnx.ExperimentEnabled("memorysnapshot")
}
