package lnx

import (
	"errors"
	"testing"
)

func TestErrSuggestsDeadVM(t *testing.T) {
	tests := []struct {
		err  error
		want bool
	}{
		{err: nil, want: false},
		{err: errors.New(`Error Domain=VZErrorDomain Code=3 Description="Invalid virtual machine state. The virtual machine is no longer live."`), want: true},
		{err: errors.New("guest request failed: connection reset by peer"), want: false},
	}

	for _, tt := range tests {
		if got := errSuggestsDeadVM(tt.err); got != tt.want {
			t.Fatalf("errSuggestsDeadVM(%v) = %v, want %v", tt.err, got, tt.want)
		}
	}
}

func TestRequestStopIsIdempotent(t *testing.T) {
	s := newAPIServer(nil, "tester", "")
	s.requestStop("test stop")
	s.requestStop("test stop again")

	select {
	case <-s.stopCh:
	default:
		t.Fatal("stopCh was not closed")
	}
}
