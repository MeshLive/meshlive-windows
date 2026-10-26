//! BMH3 consuming states enforce cryptographic verification and disclosure order.
//! Their mandatory owner hooks still need discovery/contact/admission integration.
pub mod guard;
pub mod handshake;
mod keys;
pub mod reveal;
pub use keys::ApplicationKeys;
pub mod wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Format,
    State,
    Extensions,
    Policy,
    Replay,
    Budget,
    Signature,
    Mac,
    Crypto(crate::crypto::Error),
    Aead(crate::records::Error),
}
impl From<crate::crypto::Error> for Error {
    fn from(error: crate::crypto::Error) -> Self {
        Self::Crypto(error)
    }
}
impl From<crate::records::Error> for Error {
    fn from(error: crate::records::Error) -> Self {
        Self::Aead(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BMH3 error: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Purpose {
    Link = 1,
    Endpoint = 2,
}
impl Purpose {
    pub fn domain(self) -> &'static [u8] {
        match self {
            Self::Link => b"BluetoothVideoMesh/BMH3/Link",
            Self::Endpoint => b"BluetoothVideoMesh/BMH3/EndToEnd",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    ResponderFirst,
    InitiatorFirst,
}

#[cfg(test)]
pub(crate) mod tests;
