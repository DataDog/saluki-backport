use snafu::Snafu;

use crate::buf::ReadIoBuffer;

mod length_delimited;
pub use self::length_delimited::LengthDelimitedFramer;

mod newline;
pub use self::newline::NewlineFramer;

#[derive(Debug, Snafu, Eq, PartialEq)]
#[snafu(context(suffix(false)))]
pub enum FramingError {
    #[snafu(display(
        "received invalid frame (hit EOF and couldn't not parse frame, {} bytes remaining)",
        buffer_len
    ))]
    InvalidFrame { buffer_len: usize },
}

pub trait Framer {
    /// Attempt to extract the next frame from the buffer.
    ///
    /// On success, returns a byte slice of the frame data and the number of bytes to advance the buffer.
    fn next_frame<'a, B: ReadIoBuffer>(
        &mut self, buf: &'a B, is_eof: bool,
    ) -> Result<Option<(&'a [u8], usize)>, FramingError>;
}
