use crate::record_crypto::AesGcm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Header,
    Size,
    Type,
    Flags,
    Sequence,
    Faulted,
    Crypto(i32),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Authenticated record error: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Hop,
    Endpoint,
}
impl Layer {
    pub fn header_bytes(self) -> usize {
        match self {
            Self::Hop => 17,
            Self::Endpoint => 18,
        }
    }
    pub fn maximum_ciphertext(self) -> usize {
        match self {
            Self::Hop => 4 * 1024 * 1024 + 128 + 16,
            Self::Endpoint => 4 * 1024 * 1024 + 13 + 16,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub require_opening: bool,
    pub store_forward: bool,
    pub video_fragments: bool,
    pub labels: bool,
    pub discovery_exchange: bool,
    pub maximum_ciphertext: usize,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            require_opening: false,
            store_forward: false,
            video_fragments: false,
            labels: false,
            discovery_exchange: false,
            maximum_ciphertext: usize::MAX,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub sequence: u64,
    pub flags: u8,
    pub ciphertext_bytes: usize,
}
impl Header {
    /// An untrusted structural parse. Only Opener authenticates this metadata.
    pub fn parse(layer: Layer, bytes: &[u8], policy: Policy) -> Result<Self, Error> {
        if bytes.len() != layer.header_bytes()
            || &bytes[..4]
                != match layer {
                    Layer::Hop => b"BMS1",
                    Layer::Endpoint => b"BME1",
                }
        {
            return Err(Error::Header);
        }
        let kind = bytes[4];
        let sequence = u64::from_be_bytes(bytes[5..13].try_into().unwrap());
        if sequence > i64::MAX as u64 {
            return Err(Error::Sequence);
        }
        let flags = if layer == Layer::Endpoint {
            bytes[13]
        } else {
            0
        };
        let n = layer.header_bytes();
        let length = u32::from_be_bytes(bytes[n - 4..n].try_into().unwrap()) as usize;
        if length < 16 || length > layer.maximum_ciphertext().min(policy.maximum_ciphertext) {
            return Err(Error::Size);
        }
        let known = match layer {
            Layer::Hop => {
                (4..=10).contains(&kind)
                    || kind == 11 && policy.store_forward
                    || kind == 13 && policy.require_opening && sequence == 0
            }
            Layer::Endpoint => {
                matches!(kind, 1 | 2 | 4 | 5 | 6 | 7 | 8)
                    || kind == 9 && policy.video_fragments
                    || kind == 10 && policy.labels
                    || kind == 11 && policy.discovery_exchange
            }
        };
        if !known {
            return Err(Error::Type);
        }
        if layer == Layer::Hop && policy.require_opening && sequence == 0 && kind != 13 {
            return Err(Error::Type);
        }
        if layer == Layer::Endpoint && !matches!(kind, 2 | 9) && flags != 0 {
            return Err(Error::Flags);
        }
        Ok(Self {
            kind,
            sequence,
            flags,
            ciphertext_bytes: length,
        })
    }
    fn write(self, layer: Layer, output: &mut [u8], policy: Policy) -> Result<(), Error> {
        if output.len() != layer.header_bytes() {
            return Err(Error::Header);
        }
        if layer == Layer::Hop && self.flags != 0 {
            return Err(Error::Flags);
        }
        output[..4].copy_from_slice(match layer {
            Layer::Hop => b"BMS1",
            Layer::Endpoint => b"BME1",
        });
        output[4] = self.kind;
        output[5..13].copy_from_slice(&self.sequence.to_be_bytes());
        if layer == Layer::Endpoint {
            output[13] = self.flags
        }
        let n = output.len();
        let size: u32 = self.ciphertext_bytes.try_into().map_err(|_| Error::Size)?;
        output[n - 4..].copy_from_slice(&size.to_be_bytes());
        Self::parse(layer, output, policy)?;
        Ok(())
    }
}
/// Exclusive mutable ownership serializes each direction and prevents nonce reuse.
pub struct Sealer {
    layer: Layer,
    policy: Policy,
    next: u64,
    cipher: Option<AesGcm>,
}
impl Sealer {
    pub fn new(layer: Layer, key: &[u8; 32], policy: Policy) -> Result<Self, Error> {
        Ok(Self {
            layer,
            policy,
            next: 0,
            cipher: Some(AesGcm::new(key)?),
        })
    }
    pub fn next_sequence(&self) -> u64 {
        self.next
    }
    pub fn seal(&mut self, kind: u8, flags: u8, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        if self.cipher.is_none() {
            return Err(Error::Faulted);
        }
        if plaintext.len() > self.layer.maximum_ciphertext() - 16 {
            return Err(Error::Size);
        }
        let header = Header {
            kind,
            sequence: self.next,
            flags,
            ciphertext_bytes: plaintext.len() + 16,
        };
        let n = self.layer.header_bytes();
        let mut header_bytes = [0; 18];
        header.write(self.layer, &mut header_bytes[..n], self.policy)?;
        let mut encoded = vec![0; n + plaintext.len() + 16];
        encoded[..n].copy_from_slice(&header_bytes[..n]);
        let (aad, ciphertext_and_tag) = encoded.split_at_mut(n);
        let (ciphertext, tag) = ciphertext_and_tag.split_at_mut(plaintext.len());
        match self
            .cipher
            .as_mut()
            .unwrap()
            .encrypt(self.next, aad, plaintext, ciphertext)
        {
            Ok(value) => tag.copy_from_slice(&value),
            Err(error) => {
                self.cipher = None;
                return Err(error);
            }
        }
        self.next += 1;
        Ok(encoded)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    pub header: Header,
    pub plaintext: Vec<u8>,
}
pub struct Opener {
    layer: Layer,
    policy: Policy,
    next: u64,
    cipher: Option<AesGcm>,
}
impl Opener {
    pub fn new(layer: Layer, key: &[u8; 32], policy: Policy) -> Result<Self, Error> {
        Ok(Self {
            layer,
            policy,
            next: 0,
            cipher: Some(AesGcm::new(key)?),
        })
    }
    pub fn next_sequence(&self) -> u64 {
        self.next
    }
    pub fn open(&mut self, encoded: &[u8]) -> Result<Record, Error> {
        if self.cipher.is_none() {
            return Err(Error::Faulted);
        }
        let n = self.layer.header_bytes();
        if encoded.len() < n {
            self.fault();
            return Err(Error::Header);
        }
        self.open_parts(&encoded[..n], &encoded[n..])
    }
    pub(crate) fn fault(&mut self) {
        self.cipher = None;
    }
    /// Structural/sequence checks only; this does not authenticate the header.
    pub(crate) fn inspect_header(
        &self,
        bytes: &[u8],
        maximum_ciphertext: usize,
    ) -> Result<Header, Error> {
        if self.cipher.is_none() {
            return Err(Error::Faulted);
        }
        let policy = Policy {
            maximum_ciphertext: self.policy.maximum_ciphertext.min(maximum_ciphertext),
            ..self.policy
        };
        let header = Header::parse(self.layer, bytes, policy)?;
        if match self.layer {
            Layer::Hop => header.sequence != self.next,
            Layer::Endpoint => header.sequence < self.next,
        } {
            return Err(Error::Sequence);
        }
        Ok(header)
    }
    pub(crate) fn open_parts(
        &mut self,
        header: &[u8],
        ciphertext_and_tag: &[u8],
    ) -> Result<Record, Error> {
        let result = self.open_parts_inner(header, ciphertext_and_tag);
        if result.is_err() {
            self.fault();
        }
        result
    }
    fn open_parts_inner(
        &mut self,
        header_bytes: &[u8],
        ciphertext_and_tag: &[u8],
    ) -> Result<Record, Error> {
        let header = self.inspect_header(header_bytes, usize::MAX)?;
        if ciphertext_and_tag.len() != header.ciphertext_bytes {
            return Err(Error::Size);
        }
        let end = ciphertext_and_tag.len() - 16;
        let mut plaintext = vec![0; end];
        self.cipher.as_mut().unwrap().decrypt(
            header.sequence,
            header_bytes,
            &ciphertext_and_tag[..end],
            &mut plaintext,
            ciphertext_and_tag[end..].try_into().unwrap(),
        )?;
        self.next = header.sequence + 1;
        Ok(Record { header, plaintext })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_permitted_nonce_is_used_once_then_exhaustion_refuses() {
        let mut seal = Sealer::new(Layer::Endpoint, &[0; 32], Policy::default()).unwrap();
        seal.next = i64::MAX as u64;
        let record = seal.seal(2, 0, b"last").unwrap();
        assert_eq!(
            Header::parse(Layer::Endpoint, &record[..18], Policy::default())
                .unwrap()
                .sequence,
            i64::MAX as u64
        );
        assert_eq!(seal.seal(2, 0, b"again"), Err(Error::Sequence));
    }
}
