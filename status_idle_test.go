package lnx

import (
	"testing"
	"time"
)

func TestExecStartedCancelsPendingIdleShutdown(t *testing.T) {
	s := newAPIServer(nil, "tester", "")

	s.startIdleTimer()
	time.Sleep(100 * time.Millisecond)

	s.execStarted()
	defer s.execFinished()

	time.Sleep(idleTimeout + 500*time.Millisecond)

	select {
	case <-s.idleCh:
		t.Fatal("idle shutdown fired while an exec was active")
	default:
	}
}
