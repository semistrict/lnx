//go:build darwin

package lnx

import "syscall"

func statMtime(st *syscall.Stat_t) syscall.Timespec { return st.Mtimespec }
