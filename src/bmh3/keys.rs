//! The BMH3 key schedule and one-use handshake ciphers. Identity/signature,
//! finished-MAC and owner policy checks remain the state machine's responsibility.
use super::{
    Error, Purpose,
    wire::{self, BLOCK_BYTES, SEALED_BYTES},
};
use crate::{
    bmh2::Role,
    crypto::{self, Ephemeral},
    record_crypto::AesGcm,
};
use zeroize::{Zeroize, Zeroizing};

pub fn transcript_hash(purpose: Purpose, bytes: &[u8]) -> Result<[u8; 32], Error> {
    Ok(crypto::sha256(&[purpose.domain(), &[0], bytes].concat())?)
}
pub fn label(purpose: Purpose, prk: &[u8; 32], text: &str) -> Result<Zeroizing<[u8; 32]>, Error> {
    Ok(crypto::hkdf_expand(
        prk,
        &[purpose.domain(), b"/", text.as_bytes()].concat(),
    )?)
}
pub fn signature_context(purpose: Purpose, role: Role, hash: &[u8; 32]) -> Vec<u8> {
    [
        purpose.domain(),
        if role == Role::Initiator {
            b"/Initiator signature"
        } else {
            b"/Responder signature"
        },
        &[0],
        hash,
    ]
    .concat()
}
pub struct ApplicationKeys {
    pub(super) send: Zeroizing<[u8; 32]>,
    pub(super) receive: Zeroizing<[u8; 32]>,
    pub(super) exporter: Zeroizing<[u8; 32]>,
}
impl ApplicationKeys {
    pub fn send_key(&self) -> &[u8; 32] {
        &self.send
    }
    pub fn receive_key(&self) -> &[u8; 32] {
        &self.receive
    }
    pub fn exporter(&self) -> &[u8; 32] {
        &self.exporter
    }
}
pub struct HandshakeKeys {
    purpose: Purpose,
    role: Role,
    send: Option<AesGcm>,
    receive: Option<AesGcm>,
    finished_send: Zeroizing<[u8; 32]>,
    finished_receive: Zeroizing<[u8; 32]>,
    derived: Zeroizing<[u8; 32]>,
    failed: bool,
}
impl HandshakeKeys {
    pub fn derive(
        purpose: Purpose,
        role: Role,
        ephemeral: Ephemeral,
        peer_point: &[u8; 65],
        transcript: &[u8],
    ) -> Result<Self, Error> {
        if transcript.len() != 227 {
            return Err(Error::Format);
        }
        let one = wire::Message1::parse(&transcript[..122], purpose)?;
        wire::parse_header(&transcript[122..], purpose, 2)?;
        let two_point: &[u8; 65] = transcript[130..195].try_into().unwrap();
        let own = ephemeral.public_key()?;
        let (own_point, expected_peer) = if role == Role::Initiator {
            (one.point, two_point)
        } else {
            (two_point, one.point)
        };
        if own[26..] != own_point[..] || peer_point != expected_peer || own_point == peer_point {
            return Err(Error::Format);
        }
        let dh = ephemeral.agree(&wire::spki_from_point(peer_point)?)?;
        let hs = crypto::hmac_sha256(&transcript_hash(purpose, transcript)?, &dh[..])?;
        let i2r = label(purpose, &hs, "hs i2r")?;
        let r2i = label(purpose, &hs, "hs r2i")?;
        let fi = label(purpose, &hs, "finished i")?;
        let fr = label(purpose, &hs, "finished r")?;
        let derived = label(purpose, &hs, "derived")?;
        let (send, receive, finished_send, finished_receive) = if role == Role::Initiator {
            (AesGcm::new(&i2r)?, AesGcm::new(&r2i)?, fi, fr)
        } else {
            (AesGcm::new(&r2i)?, AesGcm::new(&i2r)?, fr, fi)
        };
        Ok(Self {
            purpose,
            role,
            send: Some(send),
            receive: Some(receive),
            finished_send,
            finished_receive,
            derived,
            failed: false,
        })
    }
    fn valid_aad(&self, aad: &[u8], sender: Role) -> Result<(), Error> {
        if aad.len() != 8 && aad.len() != 105 {
            return Err(Error::Format);
        }
        let number = aad[6];
        let flags = wire::parse_header(aad, self.purpose, number)?;
        if sender == Role::Initiator && (number != 3 || aad.len() != 8)
            || sender == Role::Responder
                && !(number == 2 && flags == 1 && aad.len() == 105 || number == 4 && aad.len() == 8)
        {
            return Err(Error::Format);
        }
        Ok(())
    }
    fn fault(&mut self) {
        self.failed = true;
        self.send = None;
        self.receive = None;
        self.finished_send.zeroize();
        self.finished_receive.zeroize();
        self.derived.zeroize();
    }
    pub fn seal(
        &mut self,
        aad: &[u8],
        plaintext: &[u8; BLOCK_BYTES],
    ) -> Result<[u8; SEALED_BYTES], Error> {
        let result = (|| {
            if self.failed {
                return Err(Error::State);
            }
            self.valid_aad(aad, self.role)?;
            let mut cipher = self.send.take().ok_or(Error::State)?;
            let mut output = [0; SEALED_BYTES];
            let tag = cipher.encrypt(0, aad, plaintext, &mut output[..BLOCK_BYTES])?;
            output[BLOCK_BYTES..].copy_from_slice(&tag);
            Ok(output)
        })();
        if result.is_err() {
            self.fault();
        }
        result
    }
    pub fn open(
        &mut self,
        aad: &[u8],
        sealed: &[u8; SEALED_BYTES],
    ) -> Result<Zeroizing<[u8; BLOCK_BYTES]>, Error> {
        let result = (|| {
            if self.failed {
                return Err(Error::State);
            }
            let sender = if self.role == Role::Initiator {
                Role::Responder
            } else {
                Role::Initiator
            };
            self.valid_aad(aad, sender)?;
            let mut cipher = self.receive.take().ok_or(Error::State)?;
            let mut output = Zeroizing::new([0; BLOCK_BYTES]);
            cipher.decrypt(
                0,
                aad,
                &sealed[..BLOCK_BYTES],
                &mut output[..],
                sealed[BLOCK_BYTES..].try_into().unwrap(),
            )?;
            Ok(output)
        })();
        if result.is_err() {
            self.fault();
        }
        result
    }
    pub fn finished_mac(
        &self,
        sending: bool,
        hash: &[u8; 32],
    ) -> Result<Zeroizing<[u8; 32]>, Error> {
        if self.failed {
            return Err(Error::State);
        }
        Ok(crypto::hmac_sha256(
            if sending {
                &self.finished_send[..]
            } else {
                &self.finished_receive[..]
            },
            hash,
        )?)
    }
    /// This lower-level operation does not authenticate a transcript. Its caller
    /// must verify both blocks and complete every identity/disclosure policy gate.
    pub fn application(self, transcript: &[u8]) -> Result<ApplicationKeys, Error> {
        if self.failed || self.send.is_some() || self.receive.is_some() {
            return Err(Error::State);
        }
        let ms = crypto::hmac_sha256(
            &transcript_hash(self.purpose, transcript)?,
            &self.derived[..],
        )?;
        let a = label(self.purpose, &ms, "app i2r")?;
        let b = label(self.purpose, &ms, "app r2i")?;
        let exporter = label(self.purpose, &ms, "exporter")?;
        let (send, receive) = if self.role == Role::Initiator {
            (a, b)
        } else {
            (b, a)
        };
        Ok(ApplicationKeys {
            send,
            receive,
            exporter,
        })
    }
}
