//! Single-owner decoder format decisions from Android DecoderFormatTracker.
//! Call after source/packet admission, before enqueueing. A plan is not codec
//! admission: native input still needs SizeGuard and all session security gates.
use super::bitstream::key_size;
use crate::media::{
    CONFIG, DISCARDABLE, DISCONTINUITY, KEY_FRAME, MAX_PAYLOAD, Packet, VideoHeader,
};
use std::sync::Arc;

/// Same packet count as Android's late-join configuration cache. This receiver
/// fails instead of silently evicting a potentially required configuration.
const MAX_CONFIGURATIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    Closed,
    Header,
    Kind,
    Budget,
    Size,
}
impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Video format error: {self:?}")
    }
}
impl std::error::Error for FormatError {}
type Result<T> = std::result::Result<T, FormatError>;

#[derive(Debug)]
pub struct VideoFormat {
    pub width: u16,
    pub height: u16,
    configurations: Vec<Arc<[u8]>>,
}
impl VideoFormat {
    pub fn configurations(&self) -> &[Arc<[u8]>] {
        &self.configurations
    }
}
pub struct Rebuild {
    pub format: Arc<VideoFormat>,
    pub size_changed: bool,
    pub configuration_changed: bool,
}
/// Apply reset_queue before enqueueing this packet. Recreate the decoder from
/// rebuild before draining any of the new queue. Held config is not enqueued.
pub struct PacketPlan {
    pub queue: bool,
    pub reset_queue: bool,
    pub rebuild: Option<Rebuild>,
}
struct State {
    header: VideoHeader,
    current: Arc<VideoFormat>,
    since_key: Vec<Arc<[u8]>>,
}
pub struct FormatTracker {
    state: Option<State>,
}
impl FormatTracker {
    /// first_key is only the first admitted, key-marked video picture. No key:
    /// keep BTV3's maximum until a later key supplies a readable coded size.
    pub fn new(
        header: VideoHeader,
        configurations: &[&[u8]],
        first_key: Option<&[u8]>,
    ) -> Result<Self> {
        header.encode().map_err(|_| FormatError::Header)?;
        if configurations.len() > MAX_CONFIGURATIONS
            || configurations.iter().any(|p| p.len() > MAX_PAYLOAD)
            || first_key.is_some_and(|p| p.len() > MAX_PAYLOAD)
        {
            return Err(FormatError::Budget);
        }
        let chosen = if header.codec == 3 && !configurations.is_empty() {
            &configurations[configurations.len() - 1..]
        } else {
            configurations
        };
        let mut selected = Vec::new();
        for payload in chosen {
            if !selected.iter().any(|p: &Arc<[u8]>| p.as_ref() == *payload) {
                selected.push(Arc::from(*payload));
            }
        }
        // Android reads the initial size hint before choosing decoder CSD. A
        // size fallback must not silently replace the actual selected packet.
        let parsed = first_key.and_then(|p| key_size(header.codec, p, configurations));
        let (width, height) = checked_size(
            header,
            parsed.unwrap_or((header.width as u32, header.height as u32)),
        )?;
        Ok(Self {
            state: Some(State {
                header,
                current: Arc::new(VideoFormat {
                    width,
                    height,
                    configurations: selected,
                }),
                since_key: Vec::new(),
            }),
        })
    }
    pub fn current(&self) -> Result<&VideoFormat> {
        Ok(&self.state.as_ref().ok_or(FormatError::Closed)?.current)
    }
    /// Immutable newest format for recovery. No copy of compressed config bytes.
    pub fn recovery(&self) -> Result<Arc<VideoFormat>> {
        Ok(self
            .state
            .as_ref()
            .ok_or(FormatError::Closed)?
            .current
            .clone())
    }
    pub fn plan(&mut self, packet: Packet<'_>) -> Result<PacketPlan> {
        let mut state = self.state.take().ok_or(FormatError::Closed)?;
        let plan = state.plan(packet)?;
        self.state = Some(state);
        Ok(plan)
    }
}
impl State {
    fn plan(&mut self, packet: Packet<'_>) -> Result<PacketPlan> {
        if packet.flags & !(CONFIG | KEY_FRAME | DISCONTINUITY | DISCARDABLE) != 0 {
            return Err(FormatError::Kind);
        }
        if packet.payload.len() > MAX_PAYLOAD {
            return Err(FormatError::Budget);
        }
        if packet.flags & CONFIG != 0 {
            let existing = self
                .current
                .configurations
                .iter()
                .find(|p| p.as_ref() == packet.payload);
            let queue = existing.is_some();
            if !self.since_key.iter().any(|p| p.as_ref() == packet.payload) {
                if self.since_key.len() == MAX_CONFIGURATIONS {
                    return Err(FormatError::Budget);
                }
                self.since_key.push(
                    existing
                        .cloned()
                        .unwrap_or_else(|| Arc::from(packet.payload)),
                );
            }
            return Ok(PacketPlan {
                queue,
                reset_queue: false,
                rebuild: None,
            });
        }
        let key = packet.flags & KEY_FRAME != 0;
        let rebuild = if key { self.key(packet.payload)? } else { None };
        let reset_queue = (key && packet.flags & DISCONTINUITY != 0) || rebuild.is_some();
        Ok(PacketPlan {
            queue: true,
            reset_queue,
            rebuild,
        })
    }
    fn key(&mut self, payload: &[u8]) -> Result<Option<Rebuild>> {
        let held = std::mem::take(&mut self.since_key);
        let configuration_changed = held
            .iter()
            .any(|p| !self.current.configurations.contains(p));
        let configurations = if held.is_empty() {
            &self.current.configurations
        } else {
            &held
        };
        let parsed = key_size(self.header.codec, payload, configurations);
        let (width, height) = checked_size(
            self.header,
            parsed.unwrap_or((self.current.width as u32, self.current.height as u32)),
        )?;
        let size_changed = (width, height) != (self.current.width, self.current.height);
        if !configuration_changed && !size_changed {
            return Ok(None);
        }
        // Size-only changes discard old CSD. New CSD is held, never submitted
        // separately to the old decoder. AV1 uses the newest packet only.
        let configurations = if self.header.codec == 3 {
            held.into_iter().rev().take(1).collect()
        } else {
            held
        };
        self.current = Arc::new(VideoFormat {
            width,
            height,
            configurations,
        });
        Ok(Some(Rebuild {
            format: self.current.clone(),
            size_changed,
            configuration_changed,
        }))
    }
}
fn checked_size(header: VideoHeader, (width, height): (u32, u32)) -> Result<(u16, u16)> {
    if width == 0 || height == 0 || width > header.width as u32 || height > header.height as u32 {
        return Err(FormatError::Size);
    }
    Ok((width as u16, height as u16))
}

#[cfg(test)]
mod tests;
