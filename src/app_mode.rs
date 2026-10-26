//! Default-off developer review profile, separate from the installed app.
use std::{path::PathBuf, sync::OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { Installed, Review }

impl Mode {
    fn from_switch(value: Option<&str>) -> Self {
        if value == Some("1") { Self::Review } else { Self::Installed }
    }

    pub fn is_review(self) -> bool { self == Self::Review }

    pub fn window_class(self) -> &'static str {
        if self.is_review() { "MeshLive.ReviewWindow" } else { "MeshLive.MainWindow" }
    }

    pub fn window_title(self) -> &'static str {
        if self.is_review() { "MeshLive (Developer review)" } else { "MeshLive" }
    }

    fn data_dir(self, local: Option<PathBuf>, exe: Option<PathBuf>) -> Option<PathBuf> {
        match self {
            Self::Installed => local.map(|dir| dir.join("MeshLive")),
            Self::Review => exe.and_then(|exe| exe.parent().map(|dir| dir.join("review-data"))),
        }
    }
}

pub fn current() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| Mode::from_switch(std::env::var("MESHLIVE_REVIEW").ok().as_deref()))
}

pub fn data_dir() -> Option<PathBuf> {
    current().data_dir(std::env::var_os("LOCALAPPDATA").map(PathBuf::from), std::env::current_exe().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_opt_in_selects_review() {
        for value in [None, Some(""), Some("0"), Some("true")] {
            assert_eq!(Mode::from_switch(value), Mode::Installed);
        }
        assert!(Mode::from_switch(Some("1")).is_review());
    }

    #[test]
    fn saving_and_wiping_a_review_leaves_installed_data_intact() {
        let root = crate::sealed::tests::scratch("review-profile");
        let local = Some(root.join("local"));
        let exe = Some(root.join("bin").join("meshlive.exe"));
        let installed = Mode::Installed.data_dir(local.clone(), exe.clone()).unwrap();
        let review = Mode::Review.data_dir(local, exe).unwrap();
        let sealer = crate::sealed::tests::Plain;
        crate::sealed::write(&installed.join("settings.bin"), &sealer, b"installed").unwrap();
        crate::sealed::write(&review.join("settings.bin"), &sealer, b"review").unwrap();
        assert_eq!(crate::sealed::read(&review.join("settings.bin"), &sealer).unwrap().unwrap(), b"review");
        assert!(crate::wipe::run(&review, &mut || Ok(false)).complete);
        assert_eq!(crate::sealed::read(&installed.join("settings.bin"), &sealer).unwrap().unwrap(), b"installed");
        assert!(!review.join("settings.bin").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_review_location_never_falls_back_to_installed_data() {
        assert!(Mode::Review.data_dir(Some(PathBuf::from("local")), None).is_none());
    }
}
