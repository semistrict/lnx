use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lnx_protocol::Message;
pub(crate) use lnx_protocol::MAX_MESSAGE_SIZE;

use super::INTERRUPTED;

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

pub(crate) fn read_message_interruptible(stream: &mut UnixStream) -> Result<Option<Message>> {
    stream
        .set_read_timeout(Some(INTERRUPT_POLL_TIMEOUT))
        .context("set interruptible read timeout")?;
    loop {
        if INTERRUPTED.load(Ordering::SeqCst) {
            let _ = stream.set_read_timeout(None);
            return Ok(None);
        }
        match read_message(stream) {
            Ok(message) => {
                let _ = stream.set_read_timeout(None);
                return Ok(Some(message));
            }
            Err(e) if is_timeout_error(&e) => {}
            Err(e) => {
                let _ = stream.set_read_timeout(None);
                return Err(e);
            }
        }
    }
}

pub(crate) fn is_timeout_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                )
            })
            .unwrap_or(false)
    })
}

pub(crate) fn read_u32(stream: &mut UnixStream) -> Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).context("read u32")?;
    Ok(u32::from_be_bytes(buf))
}
