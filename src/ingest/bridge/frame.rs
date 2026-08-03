use std::io::{ErrorKind, Read};

/// Stream magic: catches a non-bridge binary piped in by mistake.
pub const MAGIC: [u8; 4] = *b"DVS\x01";
/// Current wire protocol version.
pub const VERSION: u8 = 1;
/// Reject any frame larger than this; guards against a desynced stream.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Why a frame read failed. `BadPreamble` is permanent (do not restart the
/// child); `Oversized` is transient corruption (kill + restart); `Io` covers
/// a dead pipe / partial read (restart).
#[derive(Debug)]
pub enum FrameError {
    BadPreamble,
    Oversized(u32),
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BadPreamble => write!(f, "bad stream preamble (magic/version)"),
            FrameError::Oversized(n) => write!(f, "frame length {n} exceeds cap"),
            FrameError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

/// Reads the preamble once, then length-prefixed frame bodies, from any
/// byte stream (a child's stdout in production; a `Cursor` in tests).
pub struct FrameReader<R: Read> {
    inner: R,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read and validate the 5-byte preamble. Call exactly once, first.
    pub fn read_preamble(&mut self) -> Result<(), FrameError> {
        let mut buf = [0u8; 5];
        self.inner.read_exact(&mut buf).map_err(FrameError::Io)?;
        if buf[0..4] != MAGIC || buf[4] != VERSION {
            return Err(FrameError::BadPreamble);
        }
        Ok(())
    }

    /// Read the next frame body. `Ok(None)` marks a clean end of stream
    /// (the child closed stdout on a frame boundary).
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut len_buf = [0u8; 4];
        match self.read_full(&mut len_buf)? {
            0 => return Ok(None), // clean EOF on a frame boundary
            4 => {}
            _ => {
                return Err(FrameError::Io(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "truncated frame length prefix",
                )))
            }
        }
        let len = u32::from_le_bytes(len_buf);
        if len > MAX_FRAME_BYTES {
            return Err(FrameError::Oversized(len));
        }
        let mut body = vec![0u8; len as usize];
        self.inner.read_exact(&mut body).map_err(FrameError::Io)?;
        Ok(Some(body))
    }

    /// Read up to `buf.len()` bytes, tolerating a clean 0-byte EOF. Returns the
    /// number of bytes read (0 = EOF before any byte, `buf.len()` = full).
    fn read_full(&mut self, buf: &mut [u8]) -> Result<usize, FrameError> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.inner.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(FrameError::Io(e)),
            }
        }
        Ok(filled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(bodies: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        for b in bodies {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        out
    }

    #[test]
    fn reads_preamble_then_frames_then_eof() {
        let mut r = FrameReader::new(Cursor::new(framed(&[b"hello", b"world"])));
        r.read_preamble().unwrap();
        assert_eq!(r.next_frame().unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(r.next_frame().unwrap().as_deref(), Some(&b"world"[..]));
        assert_eq!(r.next_frame().unwrap(), None);
    }

    #[test]
    fn bad_magic_is_permanent() {
        let mut bytes = framed(&[b"x"]);
        bytes[0] = b'Z';
        let mut r = FrameReader::new(Cursor::new(bytes));
        assert!(matches!(r.read_preamble(), Err(FrameError::BadPreamble)));
    }

    #[test]
    fn unknown_version_is_permanent() {
        let mut bytes = framed(&[b"x"]);
        bytes[4] = 99;
        let mut r = FrameReader::new(Cursor::new(bytes));
        assert!(matches!(r.read_preamble(), Err(FrameError::BadPreamble)));
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        let mut r = FrameReader::new(Cursor::new(bytes));
        r.read_preamble().unwrap();
        assert!(matches!(r.next_frame(), Err(FrameError::Oversized(_))));
    }

    #[test]
    fn partial_length_prefix_is_io_error() {
        // Preamble + only 2 of the 4 length bytes → not a clean EOF.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&[0u8, 0u8]);
        let mut r = FrameReader::new(Cursor::new(bytes));
        r.read_preamble().unwrap();
        assert!(matches!(r.next_frame(), Err(FrameError::Io(_))));
    }
}
