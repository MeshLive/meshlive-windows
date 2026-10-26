//! This PC's own display name (Settings > Profile > Your name), DPAPI-sealed
//! in the app folder (`profile.bin`, also on the wipe allowlist). It is sent
//! only as a signed label (CF-REC-41) in calls with verified contacts; the
//! text follows the person-name rules (CF-REC-44) and may not look like a
//! generated name, a handle or a contact marker.
use crate::sealed::{self, Dpapi, Sealer};
use std::path::{Path, PathBuf};

pub const FILE: &str = "profile.bin";
const SEAL_LABEL: &str = "MeshLive profile";
const MAGIC: &[u8; 4] = b"BPF1";

pub struct Profile {
    path: PathBuf,
    sealer: Box<dyn Sealer>,
}
impl Profile {
    pub fn new(dir: &Path, sealer: Box<dyn Sealer>) -> Self {
        Self { path: dir.join(FILE), sealer }
    }
    pub fn open_default(dir: &Path) -> Self {
        Self::new(dir, Box::new(Dpapi::new(SEAL_LABEL)))
    }
    /// The saved name (None: not set or unreadable).
    pub fn name(&self) -> Option<String> {
        let bytes = sealed::read(&self.path, self.sealer.as_ref()).ok()??;
        let text = bytes.strip_prefix(MAGIC.as_slice())?;
        String::from_utf8(text.to_vec()).ok().filter(|t| !t.is_empty())
    }
    /// Saves a name after the person-name rules; returns the canonical text.
    pub fn set_name(&self, text: &str) -> Result<String, &'static str> {
        let canonical = crate::names::normalize(crate::names::LabelKind::PersonName, text)?;
        if let Some(reason) = crate::names::reserved_reason(&canonical) {
            return Err(reason);
        }
        sealed::write(&self.path, self.sealer.as_ref(), &[MAGIC.as_slice(), canonical.as_bytes()].concat()).map_err(|_| "could not be saved")?;
        Ok(canonical)
    }
    /// Removes the name (nothing is sent afterwards).
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::tests::{Plain, scratch};

    #[test]
    fn a_saved_name_is_canonical_and_reserved_forms_are_refused() {
        let dir = scratch("profile");
        let p = Profile::new(&dir, Box::new(Plain));
        assert_eq!(p.name(), None);
        assert_eq!(p.set_name("  Asha   Rao "), Ok("Asha Rao".to_owned()));
        assert_eq!(p.name().as_deref(), Some("Asha Rao"));
        assert!(p.set_name("~Asha").is_err(), "a tilde marker is reserved");
        assert!(p.set_name("").is_err());
        assert_eq!(p.name().as_deref(), Some("Asha Rao"), "a refused name leaves the old one");
        p.clear();
        assert_eq!(p.name(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
