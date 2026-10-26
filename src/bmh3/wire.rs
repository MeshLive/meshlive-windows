use super::{Error, Order, Purpose};
use crate::{
    bmh2::{BuildTag, Role},
    crypto,
};

pub const BLOCK_BYTES: usize = 310;
pub const EXTENSION_BYTES: usize = 112;
pub const SEALED_BYTES: usize = BLOCK_BYTES + 16;

pub fn header(purpose: Purpose, number: u8, flags: u8) -> Result<[u8; 8], Error> {
    if !(1..=4).contains(&number) || flags & if number <= 2 { !1 } else { !0 } != 0 {
        return Err(Error::Format);
    }
    Ok([b'B', b'M', b'H', b'3', 1, purpose as u8, number, flags])
}
pub fn parse_header(bytes: &[u8], purpose: Purpose, number: u8) -> Result<u8, Error> {
    if bytes.len() < 8 || bytes[..8] != header(purpose, number, bytes[7])? {
        return Err(Error::Format);
    }
    Ok(bytes[7])
}
pub fn spki_from_point(point: &[u8; 65]) -> Result<[u8; 91], Error> {
    let mut spki = [0; 91];
    spki[..26].copy_from_slice(&crypto::SPKI_PREFIX[..26]);
    spki[26..].copy_from_slice(point);
    crypto::validate_agreement_public(&spki)?;
    Ok(spki)
}
pub struct Message1<'a> {
    pub may_reveal_first: bool,
    pub point: &'a [u8; 65],
    pub nonce: &'a [u8; 32],
    pub tag: &'a [u8; 16],
}
impl<'a> Message1<'a> {
    pub fn parse(bytes: &'a [u8], purpose: Purpose) -> Result<Self, Error> {
        let flags = parse_header(bytes, purpose, 1)?;
        if bytes.len() != 122 || bytes[8] != 1 {
            return Err(Error::Format);
        }
        let point = bytes[9..74].try_into().unwrap();
        spki_from_point(point)?;
        Ok(Self {
            may_reveal_first: flags != 0,
            point,
            nonce: bytes[74..106].try_into().unwrap(),
            tag: bytes[106..].try_into().unwrap(),
        })
    }
}
pub struct Message2<'a> {
    pub responder_first: bool,
    pub point: &'a [u8; 65],
    pub nonce: &'a [u8; 32],
    pub clear: &'a [u8],
    pub sealed: Option<&'a [u8; SEALED_BYTES]>,
}
impl<'a> Message2<'a> {
    pub fn parse(bytes: &'a [u8], purpose: Purpose) -> Result<Self, Error> {
        let flags = parse_header(bytes, purpose, 2)?;
        if bytes.len() != if flags == 1 { 105 + SEALED_BYTES } else { 105 } {
            return Err(Error::Format);
        }
        let point = bytes[8..73].try_into().unwrap();
        spki_from_point(point)?;
        Ok(Self {
            responder_first: flags == 1,
            point,
            nonce: bytes[73..105].try_into().unwrap(),
            clear: &bytes[..105],
            sealed: if flags == 1 {
                Some(bytes[105..].try_into().unwrap())
            } else {
                None
            },
        })
    }
}
pub fn parse_encrypted(
    bytes: &[u8],
    purpose: Purpose,
    number: u8,
) -> Result<(&[u8], &[u8; SEALED_BYTES]), Error> {
    if !matches!(number, 3 | 4)
        || parse_header(bytes, purpose, number)? != 0
        || bytes.len() != 8 + SEALED_BYTES
    {
        return Err(Error::Format);
    }
    Ok((&bytes[..8], bytes[8..].try_into().unwrap()))
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Extensions {
    pub opening: Option<Vec<u8>>,
    pub intent: Option<u8>,
    pub build_tag: Option<BuildTag>,
    pub pseudonym: bool,
    build_tag_seen: bool,
}
fn opening_valid(value: &[u8]) -> bool {
    !value.is_empty() && value[0] <= 2 && value.len() == 1 + 43 * value[0] as usize
}
impl Extensions {
    pub fn encode(&self) -> Result<[u8; EXTENSION_BYTES], Error> {
        let mut body = Vec::with_capacity(EXTENSION_BYTES);
        if let Some(opening) = &self.opening {
            if !opening_valid(opening) {
                return Err(Error::Extensions);
            }
            body.push(1);
            body.extend_from_slice(&(opening.len() as u16).to_be_bytes());
            body.extend_from_slice(opening);
        }
        if let Some(intent) = self.intent {
            body.extend_from_slice(&[2, 0, 1, intent]);
        }
        if let Some(tag) = self.build_tag {
            if !(1..=32767).contains(&tag.release) {
                return Err(Error::Extensions);
            }
            let raw = tag.release | if tag.project_key { 0x8000 } else { 0 };
            body.extend_from_slice(&[3, 0, 2]);
            body.extend_from_slice(&raw.to_be_bytes());
        }
        if self.pseudonym {
            body.extend_from_slice(&[4, 0, 0]);
        }
        let spare = EXTENSION_BYTES
            .checked_sub(body.len())
            .ok_or(Error::Extensions)?;
        if spare != 0 && spare < 3 {
            return Err(Error::Extensions);
        }
        let mut output = [0; EXTENSION_BYTES];
        if spare != 0 {
            output[1..3].copy_from_slice(&((spare - 3) as u16).to_be_bytes());
        }
        output[spare..].copy_from_slice(&body);
        Ok(output)
    }
    pub fn parse(area: &[u8]) -> Result<Self, Error> {
        if area.len() != EXTENSION_BYTES {
            return Err(Error::Extensions);
        }
        let mut result = Self::default();
        let mut cursor = 0;
        let mut last = None;
        while cursor < area.len() {
            let tlv = area.get(cursor..cursor + 3).ok_or(Error::Extensions)?;
            let kind = tlv[0];
            let length = u16::from_be_bytes([tlv[1], tlv[2]]) as usize;
            cursor += 3;
            if last.is_some_and(|previous| kind <= previous) {
                return Err(Error::Extensions);
            }
            last = Some(kind);
            let value = area.get(cursor..cursor + length).ok_or(Error::Extensions)?;
            cursor += length;
            match kind {
                0 if value.iter().all(|b| *b == 0) => {}
                1 if opening_valid(value) => result.opening = Some(value.to_vec()),
                2 if value.len() == 1 => result.intent = Some(value[0]),
                3 if value.len() == 2 => {
                    result.build_tag_seen = true;
                    let raw = u16::from_be_bytes(value.try_into().unwrap());
                    if raw & 0x7fff != 0 {
                        result.build_tag = Some(BuildTag {
                            release: raw & 0x7fff,
                            project_key: raw & 0x8000 != 0,
                        });
                    }
                }
                4 if value.is_empty() => result.pseudonym = true,
                0..=4 | 0x80..=0xff => return Err(Error::Extensions),
                _ => {}
            }
        }
        Ok(result)
    }
    /// Structural parsing alone does not enforce these authenticated placement rules.
    pub fn check_placement(
        &self,
        purpose: Purpose,
        role: Role,
        message: u8,
        order: Order,
    ) -> Result<(), Error> {
        if (self.build_tag_seen || self.build_tag.is_some()) && purpose != Purpose::Link
            || self.opening.is_some() && (role != Role::Responder || self.pseudonym)
            || self.intent.is_some()
                && (role != Role::Initiator || message != 3 || order != Order::InitiatorFirst)
        {
            return Err(Error::Extensions);
        }
        Ok(())
    }
}
