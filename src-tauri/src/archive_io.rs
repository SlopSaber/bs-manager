use std::{
    io::{self, Read, Write},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) fn copy_cancellable<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Copy was cancelled"));
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 { return Ok(()); }
        writer.write_all(&buffer[..read])?;
    }
}
