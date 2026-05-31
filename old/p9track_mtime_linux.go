//go:build linux

package lnx

import "syscall"

func statMtime(st *syscall.Stat_t) syscall.Timespec { return st.Mtim }
