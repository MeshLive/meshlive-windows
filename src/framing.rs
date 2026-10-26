//! Bounded byte-stream ingress. Each read returns at most one message, leaving
//! coalesced bytes for the session owner to process after validating that message.
use crate::records::{self, Layer, Opener, Policy, Record};

pub const MAX_HANDSHAKE_BYTES: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Length,
    Truncated,
    Faulted,
    Record(records::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Stream framing error: {self:?}")
    }
}
impl std::error::Error for Error {}

fn fill(output: &mut [u8], input: &mut &[u8]) -> usize {
    let count = output.len().min(input.len());
    output[..count].copy_from_slice(&input[..count]);
    *input = &input[count..];
    count
}

pub fn frame_handshake(message: &[u8]) -> Result<Vec<u8>, Error> {
    if !(1..=MAX_HANDSHAKE_BYTES).contains(&message.len()) {
        return Err(Error::Length);
    }
    let mut frame = Vec::with_capacity(4 + message.len());
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    Ok(frame)
}

#[derive(Default)]
pub struct HandshakeReader {
    header: [u8; 4],
    header_used: usize,
    body: Vec<u8>,
    body_used: usize,
    faulted: bool,
}
impl HandshakeReader {
    /// Returns an untrusted handshake message; the handshake state machine must
    /// validate/authenticate it before requesting the next message.
    pub fn read(&mut self, input: &mut &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if self.faulted {
            return Err(Error::Faulted);
        }
        self.header_used += fill(&mut self.header[self.header_used..], input);
        if self.header_used < self.header.len() {
            return Ok(None);
        }
        if self.body.is_empty() {
            let size = u32::from_be_bytes(self.header) as usize;
            if !(1..=MAX_HANDSHAKE_BYTES).contains(&size) {
                self.faulted = true;
                return Err(Error::Length);
            }
            self.body.resize(size, 0);
        }
        self.body_used += fill(&mut self.body[self.body_used..], input);
        if self.body_used < self.body.len() {
            return Ok(None);
        }
        self.header_used = 0;
        self.body_used = 0;
        Ok(Some(std::mem::take(&mut self.body)))
    }
    /// End-of-stream check consumes the reader so a truncated stream cannot resume.
    pub fn finish(self) -> Result<(), Error> {
        if self.faulted {
            Err(Error::Faulted)
        } else if self.header_used != 0 {
            Err(Error::Truncated)
        } else {
            Ok(())
        }
    }
}

pub struct HopReader {
    opener: Opener,
    header: [u8; 17],
    header_used: usize,
    body: Vec<u8>,
    body_used: usize,
    faulted: bool,
}
impl HopReader {
    pub fn new(key: &[u8; 32], policy: Policy) -> Result<Self, records::Error> {
        Ok(Self {
            opener: Opener::new(Layer::Hop, key, policy)?,
            header: [0; 17],
            header_used: 0,
            body: Vec::new(),
            body_used: 0,
            faulted: false,
        })
    }
    /// `maximum_ciphertext` includes the GCM tag and is checked against each
    /// completed header before body allocation. The owner must verify source and
    /// stream metadata before raising this limit (audio: 4 KiB to 28 KiB).
    /// An AEAD-authenticated record is not yet an authenticated media source.
    pub fn read(
        &mut self,
        input: &mut &[u8],
        maximum_ciphertext: usize,
    ) -> Result<Option<Record>, Error> {
        if self.faulted {
            return Err(Error::Faulted);
        }
        let result = self.read_inner(input, maximum_ciphertext);
        if result.is_err() {
            self.faulted = true;
            self.opener.fault();
            self.body = Vec::new();
        }
        result
    }
    fn read_inner(
        &mut self,
        input: &mut &[u8],
        maximum_ciphertext: usize,
    ) -> Result<Option<Record>, Error> {
        self.header_used += fill(&mut self.header[self.header_used..], input);
        if self.header_used < self.header.len() {
            return Ok(None);
        }
        if self.body.is_empty() {
            let header = self
                .opener
                .inspect_header(&self.header, maximum_ciphertext)
                .map_err(Error::Record)?;
            self.body.resize(header.ciphertext_bytes, 0);
        }
        self.body_used += fill(&mut self.body[self.body_used..], input);
        if self.body_used < self.body.len() {
            return Ok(None);
        }
        let record = self
            .opener
            .open_parts(&self.header, &self.body)
            .map_err(Error::Record)?;
        self.header_used = 0;
        self.body_used = 0;
        self.body.clear();
        Ok(Some(record))
    }
    /// Consuming the reader also destroys the native directional key handle.
    pub fn finish(self) -> Result<(), Error> {
        if self.faulted {
            Err(Error::Faulted)
        } else if self.header_used != 0 {
            Err(Error::Truncated)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_header_does_not_allocate_a_ciphertext_body() {
        let mut reader = HopReader::new(&[0; 32], Policy::default()).unwrap();
        let mut header = *b"BMS1\x06\0\0\0\0\0\0\0\0\xff\xff\xff\xff";
        assert_eq!(
            reader.read(&mut header.as_slice(), usize::MAX),
            Err(Error::Record(records::Error::Size))
        );
        assert_eq!(reader.body.capacity(), 0);
        header[13..].copy_from_slice(&16_u32.to_be_bytes());
        assert_eq!(
            reader.read(&mut header.as_slice(), usize::MAX),
            Err(Error::Faulted)
        );
    }
}
