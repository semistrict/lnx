package lnx

import (
	"encoding/gob"
	"net"
	"os"
	"syscall"
	"testing"
	"testing/synctest"

	"github.com/semistrict/lnx/internal/protocol"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestControlProtocol_ExecDelivered(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		hostConn, guestConn := net.Pipe()

		exitCh := make(chan int, 1)
		exec := &protocol.Exec{
			Args: []string{"echo", "hello"},
			CWD:  "/tmp",
			PTY:  true,
		}
		go runControlConn(hostConn, exec, exitCh, nil)

		dec := gob.NewDecoder(guestConn)
		var msg protocol.Msg
		require.NoError(t, dec.Decode(&msg))
		require.NotNil(t, msg.Exec)
		assert.Equal(t, []string{"echo", "hello"}, msg.Exec.Args)
		assert.Equal(t, "/tmp", msg.Exec.CWD)
		assert.True(t, msg.Exec.PTY)

		// Clean up: send exit so runControlConn returns.
		enc := gob.NewEncoder(guestConn)
		require.NoError(t, enc.Encode(protocol.Msg{Exit: &protocol.Exit{Code: 0}}))
		require.NoError(t, dec.Decode(&msg))
		require.NotNil(t, msg.Ack)
		synctest.Wait()
	})
}

func TestControlProtocol_ExitCode(t *testing.T) {
	for _, code := range []int{0, 1, 42, 127} {
		synctest.Test(t, func(t *testing.T) {
			hostConn, guestConn := net.Pipe()

			exitCh := make(chan int, 1)
			go runControlConn(hostConn, &protocol.Exec{Args: []string{"true"}}, exitCh, nil)

			dec := gob.NewDecoder(guestConn)
			enc := gob.NewEncoder(guestConn)

			var msg protocol.Msg
			require.NoError(t, dec.Decode(&msg))

			require.NoError(t, enc.Encode(protocol.Msg{Exit: &protocol.Exit{Code: code}}))
			require.NoError(t, dec.Decode(&msg))
			require.NotNil(t, msg.Ack)
			synctest.Wait()

			assert.Equal(t, code, <-exitCh)
		})
	}
}

func TestControlProtocol_HostClosesAfterExit(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		hostConn, guestConn := net.Pipe()

		exitCh := make(chan int, 1)
		go runControlConn(hostConn, &protocol.Exec{Args: []string{"true"}}, exitCh, nil)

		dec := gob.NewDecoder(guestConn)
		enc := gob.NewEncoder(guestConn)

		var msg protocol.Msg
		require.NoError(t, dec.Decode(&msg))
		require.NoError(t, enc.Encode(protocol.Msg{Exit: &protocol.Exit{Code: 0}}))
		require.NoError(t, dec.Decode(&msg))
		require.NotNil(t, msg.Ack)

		// After ack delivery, the host side returns and its caller closes the conn.
		synctest.Wait()
		hostConn.Close()

		var eof protocol.Msg
		err := dec.Decode(&eof)
		assert.Error(t, err)
	})
}

func TestControlProtocol_SignalForwarded(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		hostConn, guestConn := net.Pipe()

		exitCh := make(chan int, 1)
		sigCh := make(chan os.Signal, 1)
		go runControlConn(hostConn, &protocol.Exec{Args: []string{"bash"}}, exitCh, sigCh)

		dec := gob.NewDecoder(guestConn)
		enc := gob.NewEncoder(guestConn)

		// Read exec.
		var msg protocol.Msg
		dec.Decode(&msg)

		// Host sends a signal.
		sigCh <- syscall.SIGINT
		synctest.Wait()

		// Guest should receive it.
		require.NoError(t, dec.Decode(&msg))
		require.NotNil(t, msg.Signal)
		assert.Equal(t, int(syscall.SIGINT), msg.Signal.Sig)

		// Clean up.
		require.NoError(t, enc.Encode(protocol.Msg{Exit: &protocol.Exit{Code: 0}}))
		require.NoError(t, dec.Decode(&msg))
		require.NotNil(t, msg.Ack)
		synctest.Wait()
	})
}

func TestControlProtocol_UserInfo(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		hostConn, guestConn := net.Pipe()

		exitCh := make(chan int, 1)
		exec := &protocol.Exec{
			Args:    []string{"whoami"},
			User:    "ramon",
			UID:     501,
			HomeDir: "/Users/ramon",
		}
		go runControlConn(hostConn, exec, exitCh, nil)

		dec := gob.NewDecoder(guestConn)
		var msg protocol.Msg
		require.NoError(t, dec.Decode(&msg))
		assert.Equal(t, "ramon", msg.Exec.User)
		assert.Equal(t, 501, msg.Exec.UID)
		assert.Equal(t, "/Users/ramon", msg.Exec.HomeDir)

		enc := gob.NewEncoder(guestConn)
		require.NoError(t, enc.Encode(protocol.Msg{Exit: &protocol.Exit{Code: 0}}))
		require.NoError(t, dec.Decode(&msg))
		require.NotNil(t, msg.Ack)
		synctest.Wait()
	})
}
