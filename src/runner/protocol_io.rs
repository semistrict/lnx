use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
pub(crate) use lnx_protocol::MAX_MESSAGE_SIZE;
use lnx_protocol::Message;

use super::client_interrupted;

const INTERRUPT_POLL_TIMEOUT: Duration = Duration::from_millis(100);

pub(crate) fn write_message(stream: &mut UnixStream, message: &Message) -> Result<()> {
    let bytes = postcard::to_allocvec(message).context("encode protocol message")?;
    if bytes.len() > MAX_MESSAGE_SIZE as usize {
        bail!("protocol message too large: {}", bytes.len());
    }
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

pub(crate) fn read_message(stream: &mut UnixStream) -> Result<Message> {
    let len = read_u32(stream).context("read protocol length")?;
    if len > MAX_MESSAGE_SIZE {
        bail!("protocol message too large: {len}");
    }
    let mut bytes = vec![0u8; len as usize];
    stream
        .read_exact(&mut bytes)
        .with_context(|| format!("read protocol body ({len} bytes)"))?;
    postcard::from_bytes(&bytes).context("decode protocol message")
}

/// What waiting for a message ended with.
pub(crate) enum Interruptible {
    Message(Message),
    /// The client was asked to stop by a signal.
    Interrupted,
    DeadlinePassed,
}

/// Reads a message, giving up when the client is interrupted or `deadline`
/// passes. It waits for the stream to become readable and then reads a
/// whole frame, so giving up can never split a frame.
pub(crate) fn read_message_interruptible(
    stream: &mut UnixStream,
    deadline: Option<Instant>,
) -> Result<Interruptible> {
    loop {
        if client_interrupted() {
            return Ok(Interruptible::Interrupted);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(Interruptible::DeadlinePassed);
        }
        if wait_readable(stream, INTERRUPT_POLL_TIMEOUT)? {
            return read_message(stream).map(Interruptible::Message);
        }
    }
}

/// Reads a message if one starts arriving within `timeout`. Like
/// [`read_message_interruptible`], it never gives up partway through a frame.
pub(crate) fn read_message_within(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<Option<Message>> {
    if !wait_readable(stream, timeout)? {
        return Ok(None);
    }
    read_message(stream).map(Some)
}

/// Whether `stream` has data (or its end, or an error) to read within
/// `timeout`. A signal cuts the wait short.
fn wait_readable(stream: &UnixStream, timeout: Duration) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error).context("wait for the broker stream");
    }
    Ok(ready > 0)
}

pub(crate) fn read_u32(stream: &mut UnixStream) -> Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).context("read u32")?;
    Ok(u32::from_be_bytes(buf))
}
