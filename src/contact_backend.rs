//! Verified contacts behind the UI's `Backend` seam: wraps any backend (the
//! offline default or the Bluetooth owner) and answers the contact commands
//! from the DPAPI-sealed store, with Android's semantics (SecurityUi /
//! IdentityVerification): labels are unique by reading (case and look-alike
//! insensitive), a rename needs a canonical person name, forgetting removes
//! the contact and runs the presence hooks (lease revocation, claims), and a
//! damaged store is shown and kept until the user resets it.
use crate::ui::{
    backend::{Backend, Command, Update},
    model::{Contact, Handle, Person, Reach, RenameOutcome, StoreProblem},
};
use meshlive::{
    contacts::{ContactStore, Fingerprint, Method, Rename, Unreadable},
    names,
};
use std::{path::Path, time::Duration};

/// What forgetting or resetting contacts must also undo elsewhere (Android
/// `forget`: claims entry and presence leases; reset: all of them). The
/// presence store implements this; until then nothing else holds contact state.
pub trait ContactHooks {
    fn forgotten(&mut self, _fingerprint: &Fingerprint) {}
    fn reset(&mut self) {}
}
pub struct NoHooks;
impl ContactHooks for NoHooks {}

/// The app's hooks (Android `forget`): a forgotten contact's claims entry
/// goes, and a lease this PC gave it is revoked by a new generation; Reset
/// does the same for every contact.
pub struct AppHooks(pub std::path::PathBuf);
impl ContactHooks for AppHooks {
    fn forgotten(&mut self, fingerprint: &Fingerprint) {
        let node: [u8; 16] = fingerprint[..16].try_into().unwrap();
        meshlive::claims::ClaimsStore::open_default(&self.0).forget(fingerprint);
        let epoch = meshlive::dial::wall_epoch().unwrap_or(0);
        let mut vault = meshlive::presence::Vault::open_default(&self.0);
        if vault.ensure_loaded(epoch) {
            vault.forget(&node, epoch);
        }
    }
    fn reset(&mut self) {
        meshlive::claims::ClaimsStore::open_default(&self.0).reset();
        let epoch = meshlive::dial::wall_epoch().unwrap_or(0);
        let mut vault = meshlive::presence::Vault::open_default(&self.0);
        if vault.ensure_loaded(epoch) {
            vault.reset_contacts(epoch);
        }
    }
}

/// Contact handles never collide with the inner backend's person handles.
const FIRST_HANDLE: Handle = 1 << 24;

pub struct ContactBackend {
    wipe: Option<Box<dyn FnMut() -> meshlive::wipe::Report>>,
    /// Start with Windows as last applied (None: not yet this run).
    #[cfg_attr(any(test, feature = "test-hooks"), allow(dead_code))]
    startup_applied: Option<bool>,
    inner: Box<dyn Backend>,
    store: Option<ContactStore>,
    hooks: Box<dyn ContactHooks>,
    handles: Vec<(Handle, Fingerprint)>,
    next: Handle,
    out: Vec<Update>,
    problem: Option<StoreProblem>,
    started: bool,
    /// Contacts the Bluetooth owner recognised nearby / holds a lease from.
    nearby: Vec<[u8; 16]>,
    leased: Vec<[u8; 16]>,
    /// The inner backend's nearby strangers (contacts are added in front).
    strangers: Vec<Person>,
    contacts: Vec<Contact>,
    /// This PC's own name (set only by the app's start-up).
    profile: Option<meshlive::profile::Profile>,
    /// The app folder (diagnostics read its trace log) and the latest switches.
    app_dir: Option<std::path::PathBuf>,
    switches: crate::ui::model::Switches,
    /// Saved settings (set only by the app's start-up).
    settings: Option<crate::settings_store::SettingsStore>,
    /// Blocked callers (set only by the app's start-up).
    blocks: Option<meshlive::blocking::BlockList>,
    /// Test-only (`test-hooks`): the fingerprint prefix to dial once.
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    test_dial: Option<String>,
    /// Test-only (`test-hooks`): the contacts to block / unblock at start.
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    test_block: Option<String>,
    #[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
    test_unblock: Option<String>,
}

impl ContactBackend {
    /// This PC's ID for Settings (Android "Your ID"), shown once at start.
    pub fn with_own_id(mut self, id: Option<String>) -> Self {
        self.out.push(Update::OwnId(id));
        self
    }
    pub fn with_app_dir(mut self, dir: std::path::PathBuf) -> Self {
        // The last run crashed and its report waits: the shell asks once.
        if !meshlive::crash::pending(&dir).is_empty() {
            self.out.push(Update::CrashReportPending);
        }
        self.app_dir = Some(dir);
        self
    }
    pub fn with_settings(mut self, settings: crate::settings_store::SettingsStore) -> Self {
        self.settings = Some(settings);
        self
    }
    pub fn with_blocks(mut self, blocks: meshlive::blocking::BlockList) -> Self {
        self.blocks = Some(blocks);
        self
    }
    pub fn with_profile(mut self, profile: meshlive::profile::Profile) -> Self {
        let name = profile.name();
        meshlive::labels::set_own_name(name.clone());
        self.out.push(Update::OwnName { name, error: None });
        self.profile = Some(profile);
        self
    }
    /// `dir`: the app's private folder (`sealed::default_dir`); None leaves
    /// the contact list empty and reports it unreadable.
    pub fn new(inner: Box<dyn Backend>, dir: Option<&Path>, hooks: Box<dyn ContactHooks>) -> Self {
        Self::with_store(inner, dir.map(ContactStore::open_default), hooks)
    }
    /// The real wipe of this install (set only by the app's start-up; tests
    /// never have one, so a Wipe command there does nothing).
    pub fn with_wipe(mut self, wipe: Box<dyn FnMut() -> meshlive::wipe::Report>) -> Self {
        self.wipe = Some(wipe);
        self
    }
    pub fn with_store(inner: Box<dyn Backend>, store: Option<ContactStore>, hooks: Box<dyn ContactHooks>) -> Self {
        Self {
            wipe: None,
            startup_applied: None,
            inner,
            store,
            hooks,
            handles: Vec::new(),
            next: FIRST_HANDLE,
            out: Vec::new(),
            problem: None,
            started: false,
            nearby: Vec::new(),
            leased: Vec::new(),
            strangers: Vec::new(),
            contacts: Vec::new(),
            profile: None,
            blocks: None,
            settings: None,
            app_dir: None,
            switches: Default::default(),
            #[cfg(feature = "test-hooks")]
            test_dial: crate::ui::test_hooks::dial_target(),
            #[cfg(not(feature = "test-hooks"))]
            test_dial: None,
            #[cfg(feature = "test-hooks")]
            test_block: crate::ui::test_hooks::block_target(),
            #[cfg(not(feature = "test-hooks"))]
            test_block: None,
            #[cfg(feature = "test-hooks")]
            test_unblock: crate::ui::test_hooks::unblock_target(),
            #[cfg(not(feature = "test-hooks"))]
            test_unblock: None,
        }
    }
    /// Nearby rows: recognised contacts (one row per person, keyed by
    /// identity, so a rotating ID never duplicates them), then strangers.
    fn people(&self) -> Vec<Person> {
        let mut rows: Vec<Person> = self
            .contacts
            .iter()
            .filter(|c| c.nearby)
            .map(|c| Person {
                copied: false,
                id: c.id,
                name: c.name.clone(),
                reach: Reach::Direct,
                verified: true,
            })
            .collect();
        rows.extend(self.strangers.iter().cloned());
        rows
    }
    fn handle(&mut self, fingerprint: &Fingerprint) -> Handle {
        if let Some((h, _)) = self.handles.iter().find(|(_, f)| f == fingerprint) {
            return *h;
        }
        let h = self.next;
        self.next += 1;
        self.handles.push((h, *fingerprint));
        h
    }
    fn fingerprint(&self, handle: Handle) -> Option<Fingerprint> {
        self.handles.iter().find(|(h, _)| *h == handle).map(|(_, f)| *f)
    }
    /// Re-reads the store and publishes the list and its problem state.
    fn refresh(&mut self) {
        let result = match self.store.as_mut() {
            Some(store) => store.list(),
            None => Err(Unreadable::Transient),
        };
        let (list, problem) = match result {
            Ok(list) => (list, None),
            Err(Unreadable::Corrupt) => (Vec::new(), Some(StoreProblem::Corrupt)),
            Err(Unreadable::Transient) => (Vec::new(), Some(StoreProblem::Temporary)),
        };
        let blocked = self.blocks.as_ref().and_then(|b| b.list().ok()).unwrap_or_default();
        let contacts = list
            .iter()
            .map(|c| Contact {
                blocked: blocked.contains(&c.fingerprint),
                id: self.handle(&c.fingerprint),
                // A contact without a label shows by its handle until named.
                name: if c.name.is_empty() {
                    names::handle(&c.fingerprint[..6].try_into().unwrap())
                } else {
                    c.name.clone()
                },
                by_qr: c.method == Method::QrScan,
                nearby: self.nearby.contains(&c.node_id()),
                lease: self.leased.contains(&c.node_id()),
            })
            .collect();
        self.contacts = contacts;
        self.out.push(Update::Contacts(self.contacts.clone()));
        #[cfg(feature = "test-hooks")]
        if let Some(target) = self.test_unblock.clone() {
            let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
            if let (Some(fingerprint), Some(blocks)) = (list.iter().map(|c| c.fingerprint).find(|f| hex(&f[..6]) == target), self.blocks.as_ref()) {
                self.test_unblock = None;
                let ok = blocks.set(&fingerprint, false).is_ok();
                crate::radio_backend::trace("test_hook", &format!("unblock contact={target} stored={ok}"));
            }
        }
        #[cfg(feature = "test-hooks")]
        if let Some(target) = self.test_block.clone() {
            let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
            if let (Some(fingerprint), Some(blocks)) = (list.iter().map(|c| c.fingerprint).find(|f| hex(&f[..6]) == target), self.blocks.as_ref()) {
                self.test_block = None;
                let ok = blocks.set(&fingerprint, true).is_ok();
                crate::radio_backend::trace("test_hook", &format!("block contact={target} stored={ok}"));
            }
        }
        #[cfg(feature = "test-hooks")]
        if let Some(target) = self.test_dial.clone() {
            let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
            if let Some(fingerprint) = list.iter().map(|c| c.fingerprint).find(|f| hex(&f[..6]) == target) {
                self.test_dial = None;
                let contact = self.handle(&fingerprint);
                self.out.push(Update::TestDial(contact));
            }
        }
        self.out.push(Update::People(self.people()));
        if problem != self.problem || !self.started {
            self.out.push(Update::ContactsProblem(problem));
        }
        self.problem = problem;
        self.started = true;
    }
    fn rename(&mut self, contact: Handle, name: &str) -> RenameOutcome {
        let (Some(fingerprint), Some(store)) = (self.fingerprint(contact), self.store.as_mut()) else {
            return RenameOutcome::NotFound;
        };
        let others = match store.list() {
            Ok(list) => list,
            Err(_) => return RenameOutcome::StoreUnreadable,
        };
        let taken = names::label_conflict(
            name,
            others.iter().filter(|c| c.fingerprint != fingerprint).map(|c| c.name.as_str()),
        );
        if taken.is_some() {
            return RenameOutcome::NameTaken;
        }
        match store.rename(&fingerprint, name) {
            Rename::Renamed => RenameOutcome::Renamed,
            Rename::NotFound => RenameOutcome::NotFound,
            Rename::InvalidName => RenameOutcome::InvalidName,
            Rename::Unreadable(_) => RenameOutcome::StoreUnreadable,
        }
    }
}

/// Plain words for a refused name.
fn name_refusal(reason: &str) -> String {
    match reason {
        "generated name" => "This looks like a made-up nearby name. Choose another.".into(),
        "tilde at an end" | "mark character" | "separator" => "Names can't use ~, shield or lock symbols, \" - \" or \", seen\".".into(),
        "could not be saved" => "Couldn't save the name.".into(),
        _ => "Use up to 32 bytes of ordinary letters, in one script.".into(),
    }
}

impl Backend for ContactBackend {
    fn command(&mut self, command: Command) {
        match command {
            Command::ShareCrashReport => {
                if meshlive::crash::parity_enabled() {
                    let result = self.app_dir.as_deref().ok_or("No crash report is available.".to_owned())
                        .and_then(meshlive::crash::share);
                    self.out.push(Update::CrashReportShared(result));
                } else {
                    let saved = self.app_dir.as_deref().and_then(meshlive::crash::newest)
                        .ok_or("No crash report is available.".to_owned())
                        .and_then(|report| crate::diagnostics::save(&meshlive::crash::share_text(&report)).map(|p| p.display().to_string()));
                    if saved.is_ok() { if let Some(dir) = self.app_dir.as_deref() { let _ = meshlive::crash::mark_shown(dir); } }
                    self.out.push(Update::DiagnosticsSaved(saved));
                }
            }
            Command::ExportDiagnostics => {
                let device = meshlive::bluetooth::adapter_device_id();
                // The hardware ID up to the instance part (vendor and product only).
                let adapter = device.as_deref().map(|id| id.splitn(3, '#').take(2).collect::<Vec<_>>().join("#"));
                let platform = format!("{:?}", meshlive::qw_policy::current(device.as_deref()));
                let tail = self.app_dir.as_deref().filter(|_| !crate::diagnostics::private_export_enabled())
                    .map(|d| crate::diagnostics::trace_tail(d, crate::diagnostics::TRACE_LINES)).unwrap_or_default();
                let mut text = crate::diagnostics::report(env!("CARGO_PKG_VERSION"), meshlive::qw_policy::os_build(), adapter.as_deref(), &platform, &self.switches, &tail);
                // The newest crash report (scrubbed when written), now offered.
                if let Some(dir) = self.app_dir.as_deref() {
                    if let Some(crash) = meshlive::crash::newest(dir) {
                        text.push_str("
crash report (newest):
");
                        text.push_str(&meshlive::crash::share_text(&crash));
                    }
                }
                let saved = crate::diagnostics::export(self.app_dir.as_deref(), &text).map(|p| p.display().to_string());
                self.out.push(Update::DiagnosticsSaved(saved));
            }
            Command::Switches(switches) => {
                // Settings > Start with Windows: applied when it changes (never by
                // tests or test-hooks builds, which must not register themselves).
                #[cfg(not(any(test, feature = "test-hooks")))]
                if self.startup_applied != Some(switches.start_with_windows) {
                    self.startup_applied = Some(switches.start_with_windows);
                    match crate::startup::set(switches.start_with_windows, crate::packaged()) {
                        Ok(on) => crate::radio_backend::trace("start_with_windows", &format!("requested={} on={on}", switches.start_with_windows)),
                        Err(error) => crate::radio_backend::trace("start_with_windows_failed", &error),
                    }
                }
                self.switches = switches.clone();
                // Saved first, so a restart keeps what the user chose.
                if let Some(settings) = self.settings.as_ref() {
                    settings.save(&switches);
                }
                self.inner.command(Command::Switches(switches));
            }
            Command::Wipe => {
                self.settings = None;
                // The Bluetooth owner ends calls, scans and announcements and
                // drops its stores first; then nothing here holds a store.
                self.inner.command(Command::Wipe);
                self.store = None;
                self.profile = None;
                meshlive::labels::set_own_name(None);
                if let Some(wipe) = self.wipe.as_mut() {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    let report = wipe();
                    self.out.push(Update::Wiped(report));
                }
            }
            Command::SetOwnName(text) => {
                if let Some(profile) = self.profile.as_ref() {
                    let update = if text.trim().is_empty() {
                        profile.clear();
                        Update::OwnName { name: None, error: None }
                    } else {
                        match profile.set_name(&text) {
                            Ok(name) => Update::OwnName { name: Some(name), error: None },
                            Err(reason) => Update::OwnName { name: profile.name(), error: Some(name_refusal(reason)) },
                        }
                    };
                    if let Update::OwnName { name, .. } = &update {
                        meshlive::labels::set_own_name(name.clone());
                    }
                    self.out.push(update);
                }
            }
            Command::ShowSafetyCode(contact) => {
                // CF-HS-48: this PC's fingerprint against the contact's.
                let own = meshlive::identity::Identity::open_or_create()
                    .ok()
                    .and_then(|(i, _)| meshlive::bmh2::SigningIdentity::public_key(&i).ok())
                    .and_then(|spki| meshlive::crypto::sha256(&spki).ok());
                if let (Some(own), Some(theirs)) = (own, self.fingerprint(contact)) {
                    if let Ok(code) = meshlive::safety_code::pair_code(&own, &theirs) {
                        self.out.push(Update::SafetyCode { contact, code });
                    }
                }
            }
            Command::RenameContact { contact, name } => {
                let outcome = self.rename(contact, &name);
                self.out.push(Update::Renamed { contact, outcome });
                self.refresh();
            }
            Command::BlockCaller { fingerprint, blocked } => {
                if let Some(blocks) = self.blocks.as_ref() {
                    let _ = blocks.set(&fingerprint, blocked);
                }
                self.refresh();
            }
            Command::SetBlocked { contact, blocked } => {
                if let (Some(fingerprint), Some(blocks)) = (self.fingerprint(contact), self.blocks.as_ref()) {
                    let _ = blocks.set(&fingerprint, blocked);
                }
                self.refresh();
            }
            Command::ForgetContact(contact) => {
                if let (Some(fingerprint), Some(store)) = (self.fingerprint(contact), self.store.as_mut()) {
                    if store.forget(&fingerprint) {
                        self.hooks.forgotten(&fingerprint);
                    }
                }
                self.refresh();
            }
            Command::ResetContacts => {
                if let Some(store) = self.store.as_mut() {
                    if store.reset().is_ok() {
                        self.hooks.reset();
                    }
                }
                self.refresh();
            }
            Command::Call { person, voice } if person >= FIRST_HANDLE => {
                // A contact: called by its recognised rotating ID.
                match (self.fingerprint(person), self.contacts.iter().find(|c| c.id == person)) {
                    (Some(fingerprint), Some(contact)) => self.inner.command(Command::CallContact {
                        node: fingerprint[..16].try_into().unwrap(),
                        name: contact.name.clone(),
                        voice,
                    }),
                    _ => self.out.push(Update::CallSetupFailed("This contact is no longer saved".into())),
                }
            }
            Command::DismissCrashReport => {
                if let Some(dir) = self.app_dir.as_deref() {
                    let _ = meshlive::crash::mark_shown(dir);
                }
            }
            Command::OpenBluetoothSettings => {
                // Windows Settings > Bluetooth; the app never switches the radio.
                let _ = meshlive::bluetooth::radio_watch::open_settings();
            }
            other => self.inner.command(other),
        }
    }
    fn poll(&mut self) -> Vec<Update> {
        // First frame, and while a temporary failure waits for its retry.
        if !self.started || self.problem == Some(StoreProblem::Temporary) {
            self.refresh();
        }
        let mut inner = Vec::new();
        for update in self.inner.poll() {
            match update {
                Update::ContactsNearby { nearby, leased } => {
                    self.nearby = nearby;
                    self.leased = leased;
                    self.refresh();
                }
                Update::ContactsChanged => {
                    if let Some(store) = self.store.as_mut() {
                        store.reload();
                    }
                    self.refresh();
                }
                Update::People(list) => {
                    self.strangers = list;
                    self.out.push(Update::People(self.people()));
                }
                other => inner.push(other),
            }
        }
        let mut out = std::mem::take(&mut self.out);
        out.extend(inner);
        out
    }
    fn new_call_id(&mut self) -> [u8; 16] {
        self.inner.new_call_id()
    }
    fn next_due(&self) -> Option<Duration> {
        if self.problem == Some(StoreProblem::Temporary) {
            return Some(Duration::from_secs(2));
        }
        self.inner.next_due()
    }
    fn connected(&self) -> bool {
        self.inner.connected()
    }
    fn streams(&self) -> bool {
        self.inner.streams()
    }
    fn can_go_live(&self) -> bool { self.inner.can_go_live() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::backend::OfflineBackend;
    use meshlive::{contacts::Verify, sealed::Dpapi};
    use std::{cell::RefCell, path::PathBuf, rc::Rc};

    fn scratch(name: &str) -> PathBuf {
        let n = meshlive::crypto::random_nonce().unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".artifacts/test-stores")
            .join(format!("{name}-{:02x}{:02x}{:02x}", n[0], n[1], n[2]));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    #[test]
    fn the_own_name_is_saved_refused_and_cleared() {
        let dir = scratch("contact-backend-name");
        let profile = meshlive::profile::Profile::new(&dir, Box::new(Dpapi::new("io.github.meshlive.app/test/profile/1")));
        let mut b = ContactBackend::with_store(Box::new(crate::ui::backend::OfflineBackend::default()), None, Box::new(NoHooks)).with_profile(profile);
        let names = |b: &mut ContactBackend| -> Vec<(Option<String>, bool)> {
            b.poll().into_iter().filter_map(|u| match u {
                Update::OwnName { name, error } => Some((name, error.is_some())),
                _ => None,
            }).collect()
        };
        assert_eq!(names(&mut b), vec![(None, false)]);
        b.command(Command::SetOwnName("  Asha   K ".into()));
        assert_eq!(names(&mut b), vec![(Some("Asha K".into()), false)]);
        assert_eq!(meshlive::labels::own_name().as_deref(), Some("Asha K"));
        b.command(Command::SetOwnName("~Asha".into()));
        assert_eq!(names(&mut b), vec![(Some("Asha K".into()), true)]);
        b.command(Command::SetOwnName(" ".into()));
        assert_eq!(names(&mut b), vec![(None, false)]);
        assert_eq!(meshlive::labels::own_name(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
    fn store(dir: &Path) -> ContactStore {
        ContactStore::new(dir, Box::new(Dpapi::new("io.github.meshlive.app/test/contacts/1")))
    }
    #[derive(Default)]
    struct Seen(Vec<String>);
    struct Hooks(Rc<RefCell<Seen>>);
    impl ContactHooks for Hooks {
        fn forgotten(&mut self, fingerprint: &Fingerprint) {
            self.0.borrow_mut().0.push(format!("forgot {:02x}", fingerprint[0]));
        }
        fn reset(&mut self) {
            self.0.borrow_mut().0.push("reset".into());
        }
    }
    fn contacts(updates: &[Update]) -> Vec<Contact> {
        updates
            .iter()
            .rev()
            .find_map(|u| match u {
                Update::Contacts(list) => Some(list.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn rename_forget_and_reset_follow_android_and_run_the_presence_hooks() {
        let dir = scratch("contact-backend");
        let (a, b) = ([0xa1; 32], [0xb2; 32]);
        let mut s = store(&dir);
        assert_eq!(s.mark_verified(Some(&a), &a, "Alice", Method::QrScan, 1), Verify::Verified);
        assert_eq!(s.mark_verified(Some(&b), &b, "", Method::SafetyCode, 2), Verify::Verified);
        let seen = Rc::new(RefCell::new(Seen::default()));
        let mut backend = ContactBackend::with_store(
            Box::new(OfflineBackend::default()),
            Some(store(&dir)),
            Box::new(Hooks(seen.clone())),
        );
        let first = backend.poll();
        assert!(first.contains(&Update::ContactsProblem(None)));
        let list = contacts(&first);
        assert_eq!(list[0].name, "Alice");
        assert!(list[0].by_qr && !list[1].by_qr);
        // An unnamed contact shows by its handle.
        assert_eq!(list[1].name, names::handle(&[0xb2; 6]));
        let (ha, hb) = (list[0].id, list[1].id);
        // Look-alike of another contact's label: taken. Invalid and valid names.
        backend.command(Command::RenameContact { contact: hb, name: "\u{410}lice".into() });
        backend.command(Command::RenameContact { contact: hb, name: "Bob - 2".into() });
        backend.command(Command::RenameContact { contact: hb, name: "  Bob  ".into() });
        let updates = backend.poll();
        let outcomes: Vec<_> = updates
            .iter()
            .filter_map(|u| match u {
                Update::Renamed { outcome, .. } => Some(*outcome),
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes,
            vec![RenameOutcome::NameTaken, RenameOutcome::InvalidName, RenameOutcome::Renamed]
        );
        assert_eq!(contacts(&updates)[1].name, "Bob");
        backend.command(Command::ForgetContact(ha));
        let list = contacts(&backend.poll());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, hb, "handles stay stable");
        backend.command(Command::ResetContacts);
        assert!(contacts(&backend.poll()).is_empty());
        assert_eq!(seen.borrow().0, vec!["forgot a1".to_owned(), "reset".to_owned()]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An inner backend that hands out scripted updates and records commands.
    struct Scripted(Rc<RefCell<(Vec<Update>, Vec<Command>)>>);
    impl Backend for Scripted {
        fn command(&mut self, command: Command) {
            self.0.borrow_mut().1.push(command);
        }
        fn poll(&mut self) -> Vec<Update> {
            std::mem::take(&mut self.0.borrow_mut().0)
        }
        fn new_call_id(&mut self) -> [u8; 16] {
            [1; 16]
        }
    }

    #[test]
    fn a_recognised_contact_is_one_nearby_row_by_identity_and_is_called_by_its_node() {
        use crate::ui::model::{Person, Reach};
        let dir = scratch("contact-backend-nearby");
        let asha = [0xa1; 32];
        let mut s = store(&dir);
        assert_eq!(s.mark_verified(Some(&asha), &asha, "Asha", Method::QrScan, 1), Verify::Verified);
        let shared = Rc::new(RefCell::new((Vec::new(), Vec::new())));
        let mut backend = ContactBackend::with_store(Box::new(Scripted(shared.clone())), Some(store(&dir)), Box::new(NoHooks));
        backend.poll();
        let node: [u8; 16] = asha[..16].try_into().unwrap();
        let stranger = Person { id: 5, name: "Blue Otter".into(), reach: Reach::Direct, verified: false, copied: false };
        // The Bluetooth owner saw Asha (twice, across a rotation) and a stranger.
        shared.borrow_mut().0 = vec![
            Update::ContactsNearby { nearby: vec![node, node], leased: vec![node] },
            Update::People(vec![stranger.clone()]),
        ];
        let updates = backend.poll();
        let list = contacts(&updates);
        assert!(list[0].nearby && list[0].lease);
        let people = updates
            .iter()
            .rev()
            .find_map(|u| match u {
                Update::People(p) => Some(p.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(people.len(), 2, "one row for Asha, one for the stranger");
        assert_eq!((people[0].id, people[0].verified, people[0].name.as_str()), (list[0].id, true, "Asha"));
        assert_eq!(people[1], stranger);
        backend.command(Command::Call { person: list[0].id, voice: true });
        assert_eq!(
            shared.borrow().1.last(),
            Some(&Command::CallContact { node, name: "Asha".into(), voice: true })
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_store_is_reported_and_kept_until_reset() {
        let dir = scratch("contact-backend-corrupt");
        let path = dir.join("identity-verification").join("verified-contacts.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not sealed").unwrap();
        let mut backend = ContactBackend::with_store(Box::new(OfflineBackend::default()), Some(store(&dir)), Box::new(NoHooks));
        let updates = backend.poll();
        assert!(updates.contains(&Update::ContactsProblem(Some(StoreProblem::Corrupt))));
        assert_eq!(std::fs::read(&path).unwrap(), b"not sealed");
        backend.command(Command::ResetContacts);
        let updates = backend.poll();
        assert!(updates.contains(&Update::ContactsProblem(None)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_pending_crash_report_is_offered_once_and_not_now_forgets_it() {
        let dir = scratch("contact-backend-crash");
        meshlive::crash::write_in(&dir, "MeshLive crash report
kind=panic
").unwrap();
        let mut backend = ContactBackend::with_store(Box::new(OfflineBackend::default()), None, Box::new(NoHooks)).with_app_dir(dir.clone());
        assert!(backend.poll().contains(&Update::CrashReportPending));
        std::thread::sleep(std::time::Duration::from_millis(20));
        backend.command(Command::DismissCrashReport);
        assert!(meshlive::crash::pending(&dir).is_empty());
        let again = ContactBackend::with_store(Box::new(OfflineBackend::default()), None, Box::new(NoHooks)).with_app_dir(dir.clone());
        let mut again = again;
        assert!(!again.poll().contains(&Update::CrashReportPending), "offered once");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
