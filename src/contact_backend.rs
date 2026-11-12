//! Verified contacts behind the UI's `Backend` seam: wraps any backend (the
//! offline default or the Bluetooth owner) and answers the contact commands
//! from the DPAPI-sealed store, with Android's semantics (SecurityUi /
//! IdentityVerification): labels are unique by reading (case and look-alike
//! insensitive), a rename needs a canonical person name, forgetting removes
//! the contact and runs the presence hooks (lease revocation, claims), and a
//! damaged store is shown and kept until the user resets it.
use crate::ui::{
    backend::{Backend, Command, Update},
    model::{BlockedPerson, BlockedView, ClaimNote, Contact, Handle, Person, Reach, RenameOutcome, StoreProblem},
};
use meshlive::{
    contacts::{ContactStore, Fingerprint, Method, Rename, Unreadable, VerifiedContact},
    names,
};
use std::{path::Path, time::Duration};

/// What forgetting or resetting contacts must also undo elsewhere (Android
/// `forget`: claims entry and presence leases; reset: all of them). The
/// presence store implements this; until then nothing else holds contact state.
pub trait ContactHooks {
    fn forgotten(&mut self, _fingerprint: &Fingerprint) {}
    fn reset(&mut self) {}
    /// Someone was blocked (or already was): Android `PersonPolicy.onBlocked`.
    fn blocked(&mut self, _fingerprint: &Fingerprint) {}
    /// Someone was unblocked.
    fn unblocked(&mut self, _fingerprint: &Fingerprint) {}
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
    /// The radio owner revokes a blocked contact's lease in its
    /// `Command::BlocksChanged` handler, through its own presence vault (one
    /// writer), so nothing is opened here.
    fn blocked(&mut self, _fingerprint: &Fingerprint) {}
    /// This PC has no "Can see when I'm nearby" switch: an unblock turns it
    /// back on, so the next verified call delivers a lease again.
    fn unblocked(&mut self, fingerprint: &Fingerprint) {
        let node: [u8; 16] = fingerprint[..16].try_into().unwrap();
        let epoch = meshlive::dial::wall_epoch().unwrap_or(0);
        let mut vault = meshlive::presence::Vault::open_default(&self.0);
        if vault.ensure_loaded(epoch) {
            vault.set_visible(&node, true, epoch);
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
    /// What contacts call themselves (Naming v2; set only by the app's
    /// start-up). The radio owner writes the same file, so it is read fresh.
    claims: Option<meshlive::claims::ClaimsStore>,
    /// The Blocked people view last sent (sent again only when it changes).
    blocked_sent: Option<BlockedView>,
    /// Opens an email draft (`mailto:`) in the user's email app: the
    /// shell's handler in the app, a recorder in tests. False: nothing took it.
    mail: Box<dyn FnMut(&str) -> bool>,
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
    /// Where the identity key is kept, for Settings > Privacy (SEC-11).
    pub fn with_identity_key(mut self, key: Option<crate::ui::backend::IdentityKey>) -> Self {
        self.out.push(Update::IdentityKey(key));
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
    pub fn with_settings(mut self, settings: crate::settings_store::SettingsStore, load: crate::settings_store::AudienceLoad) -> Self {
        // WP4: the saved "Who sees my name" applies before any call can start
        // (until now the call policy says Nobody); a file that exists but
        // cannot be read, or an unknown or conflicting value, is Nobody (fail
        // closed). WP6NF finding 9: `load` comes from the one start-up read.
        if let crate::settings_store::AudienceLoad::Failed(reason) = load {
            crate::radio_backend::trace("names_audience", &format!("load=failed reason={reason} applied=nobody"));
        }
        meshlive::call_policy::CallPolicy::global().set_audience(load.audience());
        self.settings = Some(settings);
        self
    }
    pub fn with_blocks(mut self, blocks: meshlive::blocking::BlockList) -> Self {
        self.blocks = Some(blocks);
        self
    }
    pub fn with_claims(mut self, claims: meshlive::claims::ClaimsStore) -> Self {
        self.claims = Some(claims);
        self
    }
    /// Tests: what opens an email draft (a recorder; never a real email app).
    #[cfg(test)]
    pub fn with_mail_opener(mut self, open: Box<dyn FnMut(&str) -> bool>) -> Self {
        self.mail = open;
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
            claims: None,
            blocked_sent: None,
            mail: Box::new(default_mail_opener),
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
        // WP4/WP6: the call code checks names and presence against the newest
        // contact list (None: unreadable, so nobody is a verified contact).
        meshlive::call_policy::CallPolicy::global()
            .set_contacts(result.as_ref().ok().map(|list| list.iter().map(|c| c.fingerprint).collect()));
        let (list, problem) = match result {
            Ok(list) => (list, None),
            Err(Unreadable::Corrupt) => (Vec::new(), Some(StoreProblem::Corrupt)),
            Err(Unreadable::Transient) => (Vec::new(), Some(StoreProblem::Temporary)),
        };
        // WP6: and screens peers against the newest block list (None: unknown).
        // WP6NG finding 4: one read decides the call policy, the Blocked
        // people view and its trace alike.
        let block_read = self.blocks.as_ref().map(|b| b.entries());
        let block_list = match &block_read {
            Some(Ok(entries)) => Some(entries.iter().map(|b| b.fingerprint).collect::<Vec<_>>()),
            _ => None,
        };
        meshlive::call_policy::CallPolicy::global().set_blocked(block_list.clone());
        let blocked = block_list.unwrap_or_default();
        // Naming v2 (Android `ContactClaims`): read fresh, as the radio owner
        // writes the same file.
        let claims: Vec<Option<meshlive::claims::Claim>> = match self.claims.as_mut() {
            Some(store) => {
                store.reload();
                list.iter().map(|c| store.get(&c.fingerprint)).collect()
            }
            None => vec![None; list.len()],
        };
        let contacts = list
            .iter()
            .zip(claims)
            .map(|(c, claim)| Contact {
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
                handle: names::handle(&c.fingerprint[..6].try_into().unwrap()),
                verified_on: if c.verified_at_ms > 0 {
                    meshlive::blocking::date_text(meshlive::blocking::local_ms(c.verified_at_ms))
                } else {
                    String::new()
                },
                full_id: full_id(&c.fingerprint),
                both_ways: claim.as_ref().is_some_and(|k| k.scanned_both_ways),
                // The claim is shown, never used as the title.
                claim: claim.filter(|k| !k.claimed_name.is_empty()).map(|k| {
                    if k.has_pending_change() {
                        ClaimNote::Changed(k.claimed_name)
                    } else {
                        ClaimNote::CallsThemselves(k.claimed_name)
                    }
                }),
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
        let view = self.blocked_view(&list, block_read.as_ref());
        if self.blocked_sent.as_ref() != Some(&view) {
            if view.unreadable {
                // Design decision 2026-11-06: the block list fails open; say it in the trace.
                crate::radio_backend::trace("blocked_list", "unreadable fail_open=true");
            }
            self.blocked_sent = Some(view.clone());
            self.out.push(Update::Blocked(view));
        }
        self.out.push(Update::People(self.people()));
        if problem != self.problem || !self.started {
            self.out.push(Update::ContactsProblem(problem));
        }
        self.problem = problem;
        self.started = true;
    }
    /// Settings > Privacy > Blocked people (Android `BlockedPeopleActivity`).
    fn blocked_view(&self, list: &[VerifiedContact], read: Option<&Result<Vec<meshlive::blocking::Blocked>, ()>>) -> BlockedView {
        match read {
            Some(Ok(entries)) => BlockedView {
                unreadable: false,
                people: entries
                    .iter()
                    .map(|b| BlockedPerson { fingerprint: b.fingerprint, name: blocked_name(list, &b.fingerprint), since_ms: b.since_ms })
                    .collect(),
            },
            Some(Err(_)) => BlockedView { unreadable: true, people: Vec::new() },
            None => BlockedView::default(),
        }
    }
    /// Every block and unblock goes through here (Android
    /// `BlockedPeople.block` / `unblock`): the list, the presence side
    /// effect, the radio owner's Live filter, and what the screen says.
    fn change_block(&mut self, fingerprint: Fingerprint, blocked: bool) {
        use meshlive::blocking::BlockChange;
        let change = match self.blocks.as_ref() {
            Some(blocks) if blocked => blocks.block(&fingerprint, meshlive::blocking::now_ms()),
            Some(blocks) => blocks.unblock(&fingerprint),
            None => BlockChange::Failed,
        };
        match change {
            BlockChange::Blocked | BlockChange::AlreadyBlocked => self.hooks.blocked(&fingerprint),
            BlockChange::Unblocked => self.hooks.unblocked(&fingerprint),
            BlockChange::Full | BlockChange::Failed => {}
        }
        self.inner.command(Command::BlocksChanged);
        let list = self.store.as_mut().and_then(|s| s.list().ok()).unwrap_or_default();
        let name = blocked_name(&list, &fingerprint);
        self.out.push(Update::BlockChanged { fingerprint, name, change });
        self.refresh();
    }
    /// Naming v2 (Android `applyCallLabels`): a name label the radio owner
    /// authenticated for a real key is kept only for a saved contact, as what
    /// they call themselves. It never becomes their label and never changes
    /// how they were verified.
    fn record_claim(&mut self, fingerprint: &Fingerprint, name: &str) {
        let saved = self.store.as_mut().is_some_and(|s| matches!(s.get(fingerprint), Ok(Some(_))));
        let Some(claims) = self.claims.as_mut().filter(|_| saved) else { return };
        let result = claims.record_claim(fingerprint, name, meshlive::blocking::now_ms());
        crate::radio_backend::trace("claim_recorded", &format!("result={result:?}"));
        if matches!(result, meshlive::claims::ClaimRecord::First | meshlive::claims::ClaimRecord::Changed) {
            self.refresh();
        }
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

/// Android `IdentityFormat.groupedHex`: the fingerprint as upper-case hex in
/// groups of four, four groups per line.
fn full_id(fingerprint: &Fingerprint) -> String {
    let hex: String = fingerprint.iter().map(|b| format!("{b:02X}")).collect();
    hex.as_bytes()
        .chunks(16)
        .map(|line| line.chunks(4).map(|group| std::str::from_utf8(group).unwrap()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
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

/// What a list calls a blocked person (Android `CallBlockingUi.nameFor`):
/// your label for a verified contact, else their ID (`SSSSS-SSSSS`).
fn blocked_name(list: &[VerifiedContact], fingerprint: &Fingerprint) -> String {
    let handle = || names::handle(&fingerprint[..6].try_into().unwrap());
    list.iter()
        .find(|c| &c.fingerprint == fingerprint)
        .map(|c| if c.name.is_empty() { handle() } else { c.name.clone() })
        .unwrap_or_else(handle)
}

/// The app opens a draft with the shell's `mailto:` handler. Tests never
/// open a real email app: this opens nothing there, and tests that need an
/// opener pass their own (`with_mail_opener`).
#[cfg(not(test))]
fn default_mail_opener(uri: &str) -> bool {
    meshlive::crash::open_uri(uri)
}
#[cfg(test)]
fn default_mail_opener(_uri: &str) -> bool {
    false
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
                // WP4: "Who sees my name" is what the call code reads from now on.
                meshlive::call_policy::CallPolicy::global().set_audience(switches.names_audience);
                // Saved first, so a restart keeps what the user chose.
                if let Some(settings) = self.settings.as_ref() {
                    // WP6NF finding 7: a failed save is said; the choice still
                    // applies for this run (a more private one stays so).
                    let saved = settings.save(&switches);
                    if !saved {
                        crate::radio_backend::trace("settings_save", "failed");
                    }
                    self.out.push(Update::SettingsSaved(saved));
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
            Command::ClaimedName { contact, use_it } => {
                // Android `bindClaim`: the note is seen either way; "Use it as
                // my label" then renames them (your label, not a verification).
                let claimed = match (self.fingerprint(contact), self.claims.as_mut()) {
                    (Some(fingerprint), Some(claims)) => {
                        claims.reload();
                        let name = claims.get(&fingerprint).map(|c| c.claimed_name).filter(|n| !n.is_empty());
                        claims.acknowledge(&fingerprint);
                        name
                    }
                    _ => None,
                };
                if use_it {
                    let outcome = match claimed {
                        Some(name) => self.rename(contact, &name),
                        None => RenameOutcome::NotFound,
                    };
                    self.out.push(Update::Renamed { contact, outcome });
                }
                self.refresh();
            }
            Command::RenameContact { contact, name } => {
                let outcome = self.rename(contact, &name);
                self.out.push(Update::Renamed { contact, outcome });
                self.refresh();
            }
            Command::BlockCaller { fingerprint, blocked } => self.change_block(fingerprint, blocked),
            Command::SetBlocked { contact, blocked } => match self.fingerprint(contact) {
                Some(fingerprint) => self.change_block(fingerprint, blocked),
                None => self.refresh(),
            },
            Command::ResetBlocked => {
                let mut unblocked = Vec::new();
                if let Some(blocks) = self.blocks.as_ref() {
                    let blocked = blocks.list().unwrap_or_default();
                    let reset = blocks.reset().is_ok();
                    if reset {
                        unblocked = blocked;
                    }
                    crate::radio_backend::trace("blocked_reset", &format!("reset={reset}"));
                }
                // "Everyone is unblocked": the same visibility call an unblock
                // uses, so every contact a block hid is visible again.
                for fingerprint in &unblocked {
                    self.hooks.unblocked(fingerprint);
                }
                self.inner.command(Command::BlocksChanged);
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
            Command::OpenMailDraft(draft) => {
                // Only the fixed draft text and the version: nothing of this
                // PC's contacts, IDs, names or keys is passed in.
                let opened = meshlive::reporting::open(draft, env!("CARGO_PKG_VERSION"), &mut *self.mail);
                crate::radio_backend::trace("mail_draft", &format!("kind={draft:?} opened={opened}"));
                if !opened {
                    self.out.push(Update::MailDraftFailed);
                }
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
                Update::PeerClaimedName { fingerprint, name } => self.record_claim(&fingerprint, &name),
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
    /// WP4: the "Who sees my name" the user picked is what the call code
    /// reads once the switches are applied.
    #[test]
    fn the_name_audience_reaches_the_call_code_with_the_switches() {
        use meshlive::label_policy::NamesAudience;
        let mut b = ContactBackend::with_store(Box::new(OfflineBackend::default()), None, Box::new(NoHooks));
        for audience in [NamesAudience::Nobody, NamesAudience::VerifiedOnly, NamesAudience::Everyone] {
            b.command(Command::Switches(crate::ui::model::Switches { names_audience: audience, ..Default::default() }));
            assert_eq!(meshlive::call_policy::CallPolicy::global().snapshot().audience(), audience);
        }
    }
    /// WP4: the saved choice applies when the settings open, before any call;
    /// a first run is Android's default; a damaged file is Nobody.
    #[test]
    fn a_damaged_settings_file_names_nobody_and_a_first_run_names_everyone() {
        use meshlive::label_policy::NamesAudience;
        let dir = scratch("contact-backend-audience");
        let policy = meshlive::call_policy::CallPolicy::global();
        let open = |dir: &Path| {
            let store = crate::settings_store::SettingsStore::open_default(dir);
            let read = store.read();
            ContactBackend::with_store(Box::new(OfflineBackend::default()), None, Box::new(NoHooks)).with_settings(store, read.1)
        };
        let _first = open(&dir);
        assert_eq!(policy.snapshot().audience(), NamesAudience::Everyone, "a first run");
        std::fs::write(dir.join(crate::settings_store::FILE), b"not sealed").unwrap();
        let _damaged = open(&dir);
        assert_eq!(policy.snapshot().audience(), NamesAudience::Nobody, "fail closed");
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// WP6NF finding 7: a failed save of a more private choice is said, and the
    /// choice still applies to calls for this run.
    #[test]
    fn a_failed_settings_save_is_reported_and_the_choice_still_applies() {
        use meshlive::label_policy::NamesAudience;
        let dir = scratch("contact-backend-unsaved");
        let store = crate::settings_store::SettingsStore::open_default(&dir);
        assert!(store.save(&crate::ui::model::Switches::default()), "an earlier readable choice (everyone)");
        // The next save cannot write its temporary file.
        std::fs::create_dir_all(dir.join(format!("{}.tmp", crate::settings_store::FILE)).join("x")).unwrap();
        let read = store.read();
        let mut b = ContactBackend::with_store(Box::new(OfflineBackend::default()), None, Box::new(NoHooks)).with_settings(store, read.1);
        let _ = b.poll();
        b.command(Command::Switches(crate::ui::model::Switches { names_audience: NamesAudience::Nobody, ..Default::default() }));
        assert!(b.poll().contains(&Update::SettingsSaved(false)), "the failure is said");
        assert_eq!(meshlive::call_policy::CallPolicy::global().snapshot().audience(), NamesAudience::Nobody, "the choice applies for this run");
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// WP6NG finding 4: one block-list read per refresh: when it fails, the
    /// call policy and the Blocked people view both say so, even if a second
    /// read would have worked.
    #[test]
    fn one_block_list_read_decides_the_policy_and_the_view_alike() {
        use meshlive::sealed::Sealer;
        struct FailsOnce(std::cell::Cell<bool>, meshlive::sealed::Dpapi);
        impl Sealer for FailsOnce {
            fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, meshlive::sealed::Error> {
                self.1.seal(plain)
            }
            fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, meshlive::sealed::Error> {
                if self.0.replace(false) {
                    return Err(meshlive::sealed::Error::Unseal(0));
                }
                self.1.open(sealed)
            }
        }
        let dir = scratch("contact-backend-one-read");
        let b = [0xb3; 32];
        assert_eq!(meshlive::blocking::BlockList::open_default(&dir).block(&b, 1), meshlive::blocking::BlockChange::Blocked);
        let flaky = meshlive::blocking::BlockList::new(
            &dir,
            Box::new(FailsOnce(std::cell::Cell::new(true), meshlive::sealed::Dpapi::new("MeshLive blocked callers"))),
        );
        let mut backend = ContactBackend::with_store(Box::new(OfflineBackend::default()), None, Box::new(NoHooks)).with_blocks(flaky);
        let updates = backend.poll();
        let view = updates
            .iter()
            .rev()
            .find_map(|u| match u {
                Update::Blocked(view) => Some(view.clone()),
                _ => None,
            })
            .expect("a Blocked people view");
        let known = meshlive::call_policy::CallPolicy::global().snapshot().block_list_known();
        assert_eq!(view.unreadable, !known, "the view and the policy come from the same read");
        assert!(view.unreadable, "the failed read is said");
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// WP6/WP4: every refresh hands the call code the newest contacts and blocks.
    #[test]
    fn the_contacts_and_blocks_reach_the_call_code_on_refresh() {
        let dir = scratch("contact-backend-policy");
        let (a, b) = ([0xa1; 32], [0xb2; 32]);
        let mut s = store(&dir);
        assert_eq!(s.mark_verified(Some(&a), &a, "Alice", Method::QrScan, 1), Verify::Verified);
        let blocks = meshlive::blocking::BlockList::open_default(&dir);
        assert_eq!(blocks.block(&b, 1), meshlive::blocking::BlockChange::Blocked);
        let mut backend = ContactBackend::with_store(Box::new(OfflineBackend::default()), Some(store(&dir)), Box::new(NoHooks))
            .with_blocks(meshlive::blocking::BlockList::open_default(&dir));
        let _ = backend.poll();
        let snapshot = meshlive::call_policy::CallPolicy::global().snapshot();
        assert_eq!(snapshot.contacts, Some(vec![a]));
        assert_eq!(snapshot.blocked, Some(vec![b]));
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
        fn blocked(&mut self, fingerprint: &Fingerprint) {
            self.0.borrow_mut().0.push(format!("blocked {:02x}", fingerprint[0]));
        }
        fn unblocked(&mut self, fingerprint: &Fingerprint) {
            self.0.borrow_mut().0.push(format!("unblocked {:02x}", fingerprint[0]));
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

    /// Blocking and unblocking (Android `BlockedPeople`): each change says
    /// what it did with the person's label or ID, runs the presence hooks,
    /// tells the radio owner, and the Blocked people view follows; an
    /// unreadable list is shown, never overwritten, and Reset clears it.
    #[test]
    fn blocking_and_unblocking_say_what_happened_run_the_presence_hooks_and_tell_the_radio_owner() {
        use meshlive::blocking::{BlockChange, BlockList};
        let dir = scratch("contact-backend-blocks");
        let alice = [0xa1; 32];
        let mut s = store(&dir);
        assert_eq!(s.mark_verified(Some(&alice), &alice, "Alice", Method::QrScan, 1), Verify::Verified);
        let seen = Rc::new(RefCell::new(Seen::default()));
        let shared = Rc::new(RefCell::new((Vec::new(), Vec::new())));
        let blocks = BlockList::new(&dir, Box::new(Dpapi::new("io.github.meshlive.app/test/blocks/1")));
        let mut backend = ContactBackend::with_store(Box::new(Scripted(shared.clone())), Some(store(&dir)), Box::new(Hooks(seen.clone())))
            .with_blocks(blocks);
        let view = |updates: &[Update]| updates.iter().rev().find_map(|u| match u {
            Update::Blocked(v) => Some(v.clone()),
            _ => None,
        });
        let changes = |updates: &[Update]| updates.iter().filter_map(|u| match u {
            Update::BlockChanged { name, change, .. } => Some((name.clone(), *change)),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(view(&backend.poll()), Some(BlockedView::default()), "nobody is blocked");
        let stranger = [0xc3; 32];
        let id = names::handle(&[0xc3; 6]);
        backend.command(Command::BlockCaller { fingerprint: alice, blocked: true });
        backend.command(Command::BlockCaller { fingerprint: alice, blocked: true });
        backend.command(Command::BlockCaller { fingerprint: stranger, blocked: true });
        let updates = backend.poll();
        assert_eq!(changes(&updates), vec![
            ("Alice".to_owned(), BlockChange::Blocked),
            ("Alice".to_owned(), BlockChange::AlreadyBlocked),
            (id.clone(), BlockChange::Blocked),
        ]);
        let shown = view(&updates).unwrap();
        assert_eq!(shown.people.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), vec!["Alice".to_owned(), id]);
        assert!(shown.people.iter().all(|p| p.since_ms > 0), "when each block was made is kept");
        assert!(contacts(&updates)[0].blocked);
        assert!(shared.borrow().1.contains(&Command::BlocksChanged), "the radio owner hears of it");
        backend.command(Command::BlockCaller { fingerprint: alice, blocked: false });
        let updates = backend.poll();
        assert_eq!(changes(&updates), vec![("Alice".to_owned(), BlockChange::Unblocked)]);
        assert!(!contacts(&updates)[0].blocked);
        assert_eq!(seen.borrow().0, vec!["blocked a1".to_owned(), "blocked a1".to_owned(), "blocked c3".to_owned(), "unblocked a1".to_owned()]);

        let path = dir.join(meshlive::blocking::FILE);
        std::fs::write(&path, b"not sealed").unwrap();
        backend.command(Command::BlockCaller { fingerprint: alice, blocked: true });
        let updates = backend.poll();
        assert_eq!(changes(&updates), vec![("Alice".to_owned(), BlockChange::Failed)]);
        assert!(view(&updates).unwrap().unreadable);
        assert_eq!(std::fs::read(&path).unwrap(), b"not sealed", "never overwritten");
        backend.command(Command::ResetBlocked);
        assert_eq!(view(&backend.poll()), Some(BlockedView::default()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Reset unblocks visibility too: a contact whose block turned "Can see
    /// when I'm nearby" off is visible again, and the next verified call
    /// delivers a lease.
    #[test]
    fn resetting_the_block_list_turns_visibility_back_on() {
        use meshlive::blocking::BlockList;
        let dir = scratch("contact-backend-reset-visibility");
        let epoch = meshlive::dial::wall_epoch().unwrap_or(0);
        let (node, fingerprint) = ([0xa1u8; 16], [0xa1u8; 32]);
        let mut vault = meshlive::presence::Vault::open_default(&dir);
        assert!(vault.ensure_loaded(epoch));
        assert!(vault.deliver_lease(&node, epoch, || true).is_some());
        // The block's revocation (the radio owner does this through its own
        // vault): the contact stops recognising this PC.
        assert!(vault.set_visible(&node, false, epoch));
        drop(vault);
        let blocks = BlockList::new(&dir, Box::new(Dpapi::new("io.github.meshlive.app/test/blocks/1")));
        assert!(blocks.set(&fingerprint, true).is_ok());
        let mut backend = ContactBackend::with_store(Box::new(OfflineBackend::default()), None, Box::new(AppHooks(dir.clone())))
            .with_blocks(blocks);
        backend.command(Command::ResetBlocked);
        let mut vault = meshlive::presence::Vault::open_default(&dir);
        assert!(vault.ensure_loaded(epoch));
        assert!(vault.visible_to_them(&node), "Reset turns visibility back on");
        assert!(vault.deliver_lease(&node, epoch, || true).is_some(), "the next verified call delivers a lease");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Contact us, Report a bug and Tell MeshLive (Android `ContactEmail`):
    /// the link handed to the email app is exactly the shared builder's
    /// (address, subject, version), so no name, ID, fingerprint or code this
    /// PC holds reaches it; when no email app takes it, that is said.
    #[test]
    fn mail_drafts_hold_no_identifier_and_a_missing_email_app_is_said() {
        use meshlive::reporting::{Draft, draft_uri};
        let dir = scratch("contact-backend-mail");
        let alice = [0xa1; 32];
        let mut s = store(&dir);
        assert_eq!(s.mark_verified(Some(&alice), &alice, "Alice", Method::QrScan, 1), Verify::Verified);
        let blocks = meshlive::blocking::BlockList::new(&dir, Box::new(Dpapi::new("io.github.meshlive.app/test/blocks/1")));
        let opened = Rc::new(RefCell::new(Vec::<String>::new()));
        let works = Rc::new(std::cell::Cell::new(true));
        let (seen, ok) = (opened.clone(), works.clone());
        let mut backend = ContactBackend::with_store(Box::new(OfflineBackend::default()), Some(store(&dir)), Box::new(NoHooks))
            .with_blocks(blocks)
            .with_own_id(Some("7KQ2M-4ZD80".into()))
            .with_mail_opener(Box::new(move |uri: &str| {
                seen.borrow_mut().push(uri.to_owned());
                ok.get()
            }));
        // A report just happened: the person is blocked and named on this PC.
        backend.command(Command::BlockCaller { fingerprint: alice, blocked: true });
        let _ = backend.poll();
        for draft in [Draft::Contact, Draft::Concern] {
            backend.command(Command::OpenMailDraft(draft));
            assert!(!backend.poll().contains(&Update::MailDraftFailed), "{draft:?}: an email app took it");
        }
        let uris = opened.borrow().clone();
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(uris, vec![draft_uri(Draft::Contact, version), draft_uri(Draft::Concern, version)], "only the builder's text: nothing of this PC's state is added");
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let handle = names::handle(&[0xa1; 6]).to_lowercase();
        for uri in &uris {
            let plain = mail_decode(uri).to_lowercase();
            for secret in ["alice".to_owned(), hex(&alice), hex(&alice[..6]), handle.clone(), "7kq2m-4zd80".to_owned(), "7kq2m4zd80".to_owned()] {
                assert!(!plain.contains(&secret), "{uri} holds {secret}");
            }
        }
        works.set(false);
        backend.command(Command::OpenMailDraft(Draft::Contact));
        assert!(backend.poll().contains(&Update::MailDraftFailed), "no email app: the address is offered");
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// Percent-decoding (RFC 3986) for the draft checks.
    fn mail_decode(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
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

    /// Naming v2 (Android `ContactClaims`): a signed name from a call is kept
    /// only for a saved contact and shown as what they call themselves, never
    /// as their label; a change waits for "Use it as my label" or "Ignore";
    /// using it renames them and changes nothing about how they were verified.
    #[test]
    fn a_contacts_claimed_name_is_shown_and_becomes_the_label_only_when_chosen() {
        let dir = scratch("contact-backend-claims");
        let (asha, ravi, stranger) = ([0xa1; 32], [0xb2; 32], [0xc3; 32]);
        let mut s = store(&dir);
        assert_eq!(s.mark_verified(Some(&asha), &asha, "Asha", Method::QrScan, 1), Verify::Verified);
        assert_eq!(s.mark_verified(Some(&ravi), &ravi, "Ravi", Method::SafetyCode, 1_762_400_000_000), Verify::Verified);
        let claims = || meshlive::claims::ClaimsStore::new(&dir, Box::new(Dpapi::new("io.github.meshlive.app/test/claims/1")));
        let shared = Rc::new(RefCell::new((Vec::new(), Vec::new())));
        let mut backend = ContactBackend::with_store(Box::new(Scripted(shared.clone())), Some(store(&dir)), Box::new(NoHooks)).with_claims(claims());
        let first = contacts(&backend.poll());
        let row = first.iter().find(|c| c.name == "Ravi").unwrap().clone();
        let id = row.id;
        let ravi_row = |updates: &[Update]| contacts(updates).into_iter().find(|c| c.id == id).unwrap();
        assert_eq!(row.handle, names::handle(&ravi[..6].try_into().unwrap()));
        assert_eq!(row.full_id.split_whitespace().collect::<String>(), "B2".repeat(32), "the full ID is their fingerprint");
        assert!(!row.verified_on.is_empty() && row.claim.is_none() && !row.both_ways);
        // Signed names from calls: Ravi's is kept; a stranger's is not.
        shared.borrow_mut().0 = vec![
            Update::PeerClaimedName { fingerprint: ravi, name: "Ravi K".into() },
            Update::PeerClaimedName { fingerprint: stranger, name: "Mallory".into() },
        ];
        let updates = backend.poll();
        assert!(!updates.iter().any(|u| matches!(u, Update::PeerClaimedName { .. })), "the screens never get the raw label");
        let row = ravi_row(&updates);
        assert_eq!(row.claim, Some(ClaimNote::CallsThemselves("Ravi K".into())));
        assert_eq!(row.name, "Ravi", "your label stays the title");
        assert_eq!(claims().get(&stranger), None, "only a saved contact's name is kept");
        // A new name waits for the user; Ignore keeps your label.
        shared.borrow_mut().0 = vec![Update::PeerClaimedName { fingerprint: ravi, name: "Ravi M".into() }];
        assert_eq!(ravi_row(&backend.poll()).claim, Some(ClaimNote::Changed("Ravi M".into())));
        backend.command(Command::ClaimedName { contact: id, use_it: false });
        let row = ravi_row(&backend.poll());
        assert_eq!((row.name.as_str(), row.claim), ("Ravi", Some(ClaimNote::CallsThemselves("Ravi M".into()))));
        // Use it as my label: renamed, and still checked by code only.
        shared.borrow_mut().0 = vec![Update::PeerClaimedName { fingerprint: ravi, name: "Ravi N".into() }];
        backend.poll();
        backend.command(Command::ClaimedName { contact: id, use_it: true });
        let updates = backend.poll();
        assert!(updates.contains(&Update::Renamed { contact: id, outcome: RenameOutcome::Renamed }));
        let row = ravi_row(&updates);
        assert_eq!(row.name, "Ravi N");
        assert!(!row.by_qr, "a claimed name never verifies anyone: still checked by code only");
        assert_eq!(row.claim, Some(ClaimNote::CallsThemselves("Ravi N".into())));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
