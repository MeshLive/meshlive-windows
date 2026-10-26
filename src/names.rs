//! Text rules for names, ported from Android `LabelText`, `NameTextRules`,
//! `ConfusableSkeleton`, `ContactLabelUniqueness` and `IdentityHandle` (naming
//! design 3.2, Design G). Character data comes from the Windows ICU library
//! (icuuc.dll: general category and script, as java.lang.Character does) and
//! Windows normalization; the confusables table is the same bundled Unicode
//! 18.0.0 "MA" data and the generated-name lists are the same frozen names-v2
//! files, each checked against Android's SHA-256 pins. The Unicode version of
//! the Windows ICU can differ from Android's, so a just-assigned character may
//! be judged differently on the two platforms.
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};
use windows::Win32::Globalization::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    PersonName,
    StreamTitle,
    CallSubject,
}
impl LabelKind {
    pub fn max_bytes(self) -> usize {
        match self {
            Self::PersonName => 32,
            Self::StreamTitle => 48,
            Self::CallSubject => 64,
        }
    }
}

/// Longest run of combining marks after one base character (stops "Zalgo").
pub const MAX_COMBINING_RUN: usize = 3;
const CHECK_MARKS: [u32; 5] = [0x2713, 0x2714, 0x2705, 0x2611, 0x1F5F8];
const TILDES: [u32; 6] = [0x007E, 0x223C, 0xFF5E, 0x02DC, 0x2053, 0x301C];
const SYMBOLS: [u32; 5] = [0x1F6E1, 0x1F512, 0x1F513, 0x1F510, 0x1F50F];

fn category(c: char) -> UCharCategory {
    UCharCategory(i32::from(unsafe { u_charType(c as i32) }))
}
fn script(c: char) -> UScriptCode {
    let mut error = UErrorCode(0);
    unsafe { uscript_getScript(c as i32, &mut error) }
}
/// java.lang.Character.isWhitespace || isSpaceChar.
fn space(c: char) -> bool {
    unsafe { u_isWhitespace(c as i32) != 0 || u_isJavaSpaceChar(c as i32) != 0 }
}
/// Control, format, private use, surrogate, unassigned, line/paragraph separator.
pub fn rejected_type(c: char) -> bool {
    [
        U_CONTROL_CHAR,
        U_FORMAT_CHAR,
        U_PRIVATE_USE_CHAR,
        U_SURROGATE,
        U_UNASSIGNED,
        U_LINE_SEPARATOR,
        U_PARAGRAPH_SEPARATOR,
    ]
    .contains(&category(c))
}

/// Windows normalization; None for invalid text.
pub fn normalize_form(text: &str, form: NORM_FORM) -> Option<String> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return Some(String::new());
    }
    let mut out = vec![0u16; wide.len() * 4 + 16];
    loop {
        let written = unsafe { NormalizeString(form, &wide, Some(&mut out)) };
        if written > 0 {
            return String::from_utf16(&out[..written as usize]).ok();
        }
        // A negative result estimates the size needed.
        if written < 0 && (-written) as usize > out.len() {
            out.resize((-written) as usize, 0);
            continue;
        }
        return None;
    }
}
pub fn nfc(text: &str) -> Option<String> {
    normalize_form(text, NormalizationC)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending = false;
    for c in text.trim_matches(space).chars() {
        if space(c) {
            pending = true;
        } else {
            if pending {
                out.push(' ');
            }
            pending = false;
            out.push(c);
        }
    }
    out
}

/// The canonical label text, or the short reason it is refused.
pub fn normalize(kind: LabelKind, text: &str) -> Result<String, &'static str> {
    let normalized = nfc(&collapse_whitespace(text)).ok_or("invalid")?;
    if normalized.is_empty() {
        return Err("empty");
    }
    if normalized.len() > kind.max_bytes() {
        return Err("too long");
    }
    let mut run = 0;
    for c in normalized.chars() {
        if rejected_type(c) {
            return Err("control, format or unassigned character");
        }
        if CHECK_MARKS.contains(&(c as u32)) {
            return Err("check mark");
        }
        if matches!(category(c), U_NON_SPACING_MARK | U_ENCLOSING_MARK) {
            run += 1;
            if run > MAX_COMBINING_RUN {
                return Err("stacked combining marks");
            }
        } else {
            run = 0;
        }
    }
    if !scripts_allowed(&normalized) {
        return Err("mixed scripts");
    }
    if looks_like_handle(&normalized) {
        return Err("looks like a handle");
    }
    if kind == LabelKind::PersonName && reserved_reason(&normalized).is_some() {
        return Err("reserved_form");
    }
    Ok(normalized)
}
/// Receiver side: valid and already canonical.
pub fn is_canonical(kind: LabelKind, text: &str) -> bool {
    normalize(kind, text).is_ok_and(|n| n == text)
}

/// UTS #39 Moderately Restrictive (Android `scriptsAllowed`).
pub fn scripts_allowed(text: &str) -> bool {
    let mut scripts = HashSet::new();
    for c in text.chars() {
        let s = script(c);
        if s != USCRIPT_COMMON && s != USCRIPT_INHERITED && s != USCRIPT_UNKNOWN {
            scripts.insert(s.0);
        }
    }
    if scripts.len() <= 1 {
        return true;
    }
    let (latin, han) = (USCRIPT_LATIN.0, USCRIPT_HAN.0);
    let cjk = [
        vec![latin, han, USCRIPT_HIRAGANA.0, USCRIPT_KATAKANA.0],
        vec![latin, han, USCRIPT_BOPOMOFO.0],
        vec![latin, han, USCRIPT_HANGUL.0],
    ];
    if cjk.iter().any(|set| scripts.iter().all(|s| set.contains(s))) {
        return true;
    }
    if scripts.contains(&latin) && scripts.len() == 2 {
        let other = *scripts.iter().find(|s| **s != latin).unwrap();
        return other != USCRIPT_CYRILLIC.0 && other != USCRIPT_GREEK.0;
    }
    false
}

// ---------------------------------------------------------------- handles

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// DESIGN 3.1: `prefix48 << 2` as 10 Crockford Base32 symbols, `SSSSS-SSSSS`.
pub fn handle(prefix: &[u8; 6]) -> String {
    let value = prefix.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b)) << 2;
    let symbols: Vec<char> = (0..10)
        .rev()
        .map(|i| CROCKFORD[((value >> (i * 5)) & 0x1f) as usize] as char)
        .collect();
    format!(
        "{}-{}",
        symbols[..5].iter().collect::<String>(),
        symbols[5..].iter().collect::<String>()
    )
}
fn canonical_symbols(text: &str) -> Option<String> {
    let mut out = String::new();
    for c in text.chars() {
        let upper = c.to_ascii_uppercase();
        match upper {
            '-' | ' ' => {}
            'I' | 'L' => out.push('1'),
            'O' => out.push('0'),
            _ if upper.is_ascii() && CROCKFORD.contains(&(upper as u8)) => out.push(upper),
            _ => return None,
        }
    }
    Some(out)
}
/// A typed or read-aloud handle: the 6-byte prefix, or None.
pub fn parse_handle(text: &str) -> Option<[u8; 6]> {
    let symbols = canonical_symbols(text)?;
    if symbols.len() != 10 {
        return None;
    }
    let value = symbols.bytes().fold(0u64, |v, s| {
        (v << 5) | CROCKFORD.iter().position(|c| *c == s).unwrap() as u64
    });
    if value & 0b11 != 0 {
        return None;
    }
    let prefix = value >> 2;
    Some(prefix.to_be_bytes()[2..].try_into().unwrap())
}
fn parse_short_handle(text: &str) -> bool {
    canonical_symbols(text).is_some_and(|s| s.len() == 5)
}
fn handle_shaped(folded: &str) -> bool {
    let b = folded.as_bytes();
    b.len() == 11
        && b[5] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 5 || c.is_ascii_digit() || c.is_ascii_lowercase())
}
/// R6: only handle-shaped text (or text with a digit that parses as a handle).
pub fn looks_like_handle(text: &str) -> bool {
    let folded = normalize_form(text, NormalizationKC)
        .unwrap_or_else(|| text.to_owned())
        .to_lowercase();
    if handle_shaped(&folded) {
        return true;
    }
    if !folded.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    parse_handle(&folded).is_some() || parse_short_handle(&folded)
}

// ---------------------------------------------------------------- confusables

pub struct Confusables {
    map: HashMap<u32, Vec<u32>>,
}
const CONFUSABLES: &str = include_str!("../assets/uts39/confusables_ma.txt");
impl Confusables {
    pub fn parse(text: &str) -> Self {
        let mut map = HashMap::with_capacity(8192);
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split(' ').filter(|f| !f.is_empty()).collect();
            if fields.len() < 2 {
                continue;
            }
            let Ok(source) = u32::from_str_radix(fields[0], 16) else {
                continue;
            };
            let targets: Option<Vec<u32>> = fields[1..].iter().map(|f| u32::from_str_radix(f, 16).ok()).collect();
            if let Some(targets) = targets {
                map.insert(source, targets);
            }
        }
        Self { map }
    }
    /// The bundled Unicode table (about 6,700 entries).
    pub fn bundled() -> &'static Self {
        static TABLE: OnceLock<Confusables> = OnceLock::new();
        TABLE.get_or_init(|| Self::parse(CONFUSABLES))
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// UTS #39: NFD, drop default-ignorables, map, NFD.
    pub fn skeleton(&self, text: &str) -> String {
        let decomposed = normalize_form(text, NormalizationD).unwrap_or_else(|| text.to_owned());
        let mut mapped = String::with_capacity(decomposed.len());
        for c in decomposed.chars() {
            let v = c as u32;
            if category(c) == U_FORMAT_CHAR
                || (0xFE00..=0xFE0F).contains(&v)
                || (0xE0100..=0xE01EF).contains(&v)
                || v == 0x034F
            {
                continue;
            }
            match self.map.get(&v) {
                Some(targets) => mapped.extend(targets.iter().filter_map(|t| char::from_u32(*t))),
                None => mapped.push(c),
            }
        }
        normalize_form(&mapped, NormalizationD).unwrap_or(mapped)
    }
}

/// The two readings compared for sameness (skeleton then fold, fold then skeleton).
pub fn readings(text: &str) -> [String; 2] {
    let table = Confusables::bundled();
    let trimmed = text.trim();
    [
        table.skeleton(trimmed).to_lowercase(),
        table.skeleton(&trimmed.to_lowercase()).to_lowercase(),
    ]
}
/// Case- and look-alike-insensitive sameness.
pub fn same_reading(a: &str, b: &str) -> bool {
    if a.trim().is_empty() || b.trim().is_empty() {
        return false;
    }
    let (ra, rb) = (readings(a), readings(b));
    ra.iter().any(|x| rb.contains(x))
}
/// Android ContactLabelUniqueness.conflict: the existing label `candidate`
/// reads the same as, if any.
pub fn label_conflict<'a>(candidate: &str, existing: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    if candidate.trim().is_empty() {
        return None;
    }
    existing.into_iter().find(|other| same_reading(candidate, other))
}

fn reduced_skeleton(text: &str) -> String {
    let skeleton = Confusables::bundled().skeleton(text.trim()).to_lowercase();
    nfc(&skeleton)
        .unwrap_or(skeleton)
        .chars()
        .filter(|c| {
            !(space(*c)
                || [
                    U_CONNECTOR_PUNCTUATION,
                    U_DASH_PUNCTUATION,
                    U_START_PUNCTUATION,
                    U_END_PUNCTUATION,
                    U_INITIAL_PUNCTUATION,
                    U_FINAL_PUNCTUATION,
                    U_OTHER_PUNCTUATION,
                    U_MATH_SYMBOL,
                    U_MODIFIER_SYMBOL,
                ]
                .contains(&category(*c)))
        })
        .collect()
}

// ---------------------------------------------------------------- generated names

pub const ADJECTIVES_SHA256: &str = "d832789668051db146496979eaf9d362b90f43c55d51febb9c825dc1e0417b46";
pub const NOUNS_SHA256: &str = "1560dd97fdb19313fcdcfd6335c454d888d298ca2da595fc4c032d051ab4a39c";
pub const BLOCKED_PAIRS_SHA256: &str = "381d7404fe637a61ace734d6569103f159de2e27d2ea62adc54dae4f9acfa304";
const ADJECTIVES: &str = include_str!("../assets/names/adjectives-v2.txt");
const NOUNS: &str = include_str!("../assets/names/nouns-v2.txt");
const BLOCKED_PAIRS: &str = include_str!("../assets/names/blocked-pairs-v2.txt");

pub struct NameLists {
    pub adjectives: Vec<String>,
    pub nouns: Vec<String>,
    pub blocked: HashSet<usize>,
    reduced_adjectives: Vec<String>,
    reduced_nouns: HashSet<String>,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
impl NameLists {
    /// names-v2, installed only when all three files match the frozen pins.
    pub fn installed() -> Option<&'static Self> {
        static LISTS: OnceLock<Option<NameLists>> = OnceLock::new();
        LISTS
            .get_or_init(|| {
                let pinned = |text: &str, pin: &str| crate::crypto::sha256(text.as_bytes()).is_ok_and(|h| hex(&h) == pin);
                if !(pinned(ADJECTIVES, ADJECTIVES_SHA256)
                    && pinned(NOUNS, NOUNS_SHA256)
                    && pinned(BLOCKED_PAIRS, BLOCKED_PAIRS_SHA256))
                {
                    return None;
                }
                Self::parse(ADJECTIVES, NOUNS, BLOCKED_PAIRS)
            })
            .as_ref()
    }
    fn parse(adjectives: &str, nouns: &str, blocked: &str) -> Option<Self> {
        let words = |t: &str| -> Option<Vec<String>> {
            let list: Vec<String> = t.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
            let ok = list.len() == 256
                && list.iter().all(|w| (3..=7).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_lowercase()))
                && list.windows(2).all(|p| p[0] < p[1]);
            ok.then_some(list)
        };
        let (adjectives, nouns) = (words(adjectives)?, words(nouns)?);
        let mut set = HashSet::new();
        for line in blocked.lines().filter(|l| !l.is_empty()) {
            let (a, n) = line.split_once(' ')?;
            let a = adjectives.iter().position(|w| w == a)?;
            let n = nouns.iter().position(|w| w == n)?;
            set.insert(a * 256 + n);
        }
        Some(Self {
            reduced_adjectives: adjectives.iter().map(|w| reduced_skeleton(w)).collect(),
            reduced_nouns: nouns.iter().map(|w| reduced_skeleton(w)).collect(),
            adjectives,
            nouns,
            blocked: set,
        })
    }
    /// Rule 3: a reduced reading equals adjective + noun.
    pub fn matches_generated(&self, text: &str) -> bool {
        let trimmed = text.trim();
        [reduced_skeleton(trimmed), reduced_skeleton(&trimmed.to_lowercase())]
            .iter()
            .any(|reduced| {
                !reduced.is_empty()
                    && self.reduced_adjectives.iter().any(|a| {
                        reduced.len() > a.len()
                            && reduced.starts_with(a.as_str())
                            && self.reduced_nouns.contains(&reduced[a.len()..])
                    })
            })
    }
}

/// NameTextRules: a person name may not imitate the marks or a generated name.
pub fn reserved_reason(text: &str) -> Option<&'static str> {
    let (first, last) = (text.chars().next()? as u32, text.chars().last()? as u32);
    if TILDES.contains(&first) || TILDES.contains(&last) {
        return Some("tilde at an end");
    }
    if text.chars().any(|c| c == '~' || SYMBOLS.contains(&(c as u32))) {
        return Some("mark character");
    }
    let lower = text.to_lowercase();
    if text.contains(" - ") || lower.split(',').skip(1).any(|part| part.trim_start().starts_with("seen")) {
        return Some("separator");
    }
    if NameLists::installed().is_some_and(|lists| lists.matches_generated(text)) {
        return Some("generated name");
    }
    None
}

// ---------------------------------------------------------------- stranger names (CF-PRIV-21/22)

/// What a generated stranger name is derived from: exactly the 6-byte RDI of
/// a rotating advert, or the 16-byte node ID of a stranger session's
/// throwaway key; never a real node ID, fingerprint, prefix or address.
#[derive(Debug, Clone, Copy)]
pub enum NameInput<'a> {
    Rdi(&'a [u8]),
    Throwaway(&'a [u8]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrangerName {
    pub name: String,
    pub chip: u8,
    pub number: u8,
    /// The last two input bytes (the second collision suffix).
    pub tail: [u8; 2],
}

const STRANGER_DOMAIN: &[u8] = b"BluetoothVideoMesh/Naming/StrangerName/1";

fn stranger_digest(input: &[u8], attempt: u8) -> Option<[u8; 32]> {
    let mut data = [STRANGER_DOMAIN, &[0u8], input].concat();
    if attempt > 0 {
        data.push(attempt);
    }
    crate::crypto::sha256(&data).ok()
}

/// names-v2: `adjectives[d0] + " " + nouns[d1]`, chip `d2 & 7`, number
/// `d3 % 100`; a blocked pair re-hashes with an attempt byte (1, 2, ...) up to
/// 16 attempts, then the last stands. None: wrong input size or no lists.
pub fn stranger_name(input: NameInput) -> Option<StrangerName> {
    let bytes = match input {
        NameInput::Rdi(b) if b.len() == 6 => b,
        NameInput::Throwaway(b) if b.len() == 16 => b,
        _ => return None,
    };
    let lists = NameLists::installed()?;
    let mut d = [0u8; 32];
    for attempt in 0..16u8 {
        d = stranger_digest(bytes, attempt)?;
        if !lists.blocked.contains(&(usize::from(d[0]) * 256 + usize::from(d[1]))) {
            break;
        }
    }
    Some(StrangerName {
        name: format!("{} {}", lists.adjectives[usize::from(d[0])], lists.nouns[usize::from(d[1])]),
        chip: d[2] & 7,
        number: d[3] % 100,
        tail: [bytes[bytes.len() - 2], bytes[bytes.len() - 1]],
    })
}

/// CF-PRIV-22: when two or more rows with different IDs share a name, each
/// gets " " + its two-digit number; rows still equal then get "-" + the last
/// two input bytes as hex. Suffixes come from the ID, never arrival order.
/// `rows`: (row ID bytes, name), in display order; returns the labels.
pub fn disambiguate(rows: &[(Vec<u8>, StrangerName)]) -> Vec<String> {
    let distinct = |name: &str| {
        let mut ids: Vec<&Vec<u8>> = rows.iter().filter(|(_, n)| n.name == name).map(|(id, _)| id).collect();
        ids.sort();
        ids.dedup();
        ids.len()
    };
    let first: Vec<String> = rows
        .iter()
        .map(|(_, n)| if distinct(&n.name) >= 2 { format!("{} {:02}", n.name, n.number) } else { n.name.clone() })
        .collect();
    rows.iter()
        .zip(&first)
        .map(|((id, n), label)| {
            let clash = rows.iter().zip(&first).any(|((other, _), l)| l == label && other != id);
            if clash { format!("{label}-{:02x}{:02x}", n.tail[0], n.tail[1]) } else { label.clone() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_android_text_rules() {
        let p = LabelKind::PersonName;
        assert_eq!(normalize(p, "  Ana   Maria "), Ok("Ana Maria".into()));
        assert_eq!(normalize(p, "Cafe\u{301}"), Ok("Caf\u{e9}".into()));
        assert_eq!(normalize(p, ""), Err("empty"));
        assert_eq!(normalize(p, &"x".repeat(33)), Err("too long"));
        assert_eq!(normalize(LabelKind::StreamTitle, &"x".repeat(48)).map(|s| s.len()), Ok(48));
        assert!(normalize(p, "a\u{200b}b").is_err());
        assert!(normalize(p, "a\u{e000}").is_err());
        // Unassigned (U+0378) and a noncharacter are refused.
        assert!(normalize(p, "a\u{378}").is_err());
        assert!(normalize(p, "a\u{fdd0}").is_err());
        assert_eq!(normalize(p, "Ana \u{2713}"), Err("check mark"));
        // NFC folds the first mark into the base, so five marks leave a run of four.
        assert_eq!(normalize(p, "e\u{301}\u{302}\u{303}\u{304}\u{305}"), Err("stacked combining marks"));
        // Mixed scripts: Latin+Cyrillic refused, Latin+Han+Hiragana allowed, all-Cyrillic allowed.
        assert_eq!(normalize(p, "Ali\u{441}e"), Err("mixed scripts"));
        assert!(normalize(p, "Ken \u{5c71}\u{7530} \u{3072}").is_ok());
        assert!(normalize(p, "\u{410}\u{43d}\u{43d}\u{430}").is_ok());
        // Handles.
        assert_eq!(normalize(p, "abcde-12345"), Err("looks like a handle"));
        assert_eq!(normalize(p, "Brave Tiger").as_deref(), Ok("Brave Tiger"));
        assert_eq!(normalize(p, "Mike Foster").as_deref(), Ok("Mike Foster"));
        // Reserved forms.
        assert_eq!(normalize(p, "~Ana"), Err("reserved_form"));
        assert_eq!(normalize(p, "Ana \u{1f6e1}"), Err("reserved_form"));
        assert_eq!(normalize(p, "Ana - Bob"), Err("reserved_form"));
        assert_eq!(normalize(p, "Ana, seen"), Err("reserved_form"));
        assert!(is_canonical(p, "Ana Maria"));
        assert!(!is_canonical(p, "Ana  Maria"));
    }

    #[test]
    fn handles_round_trip_like_android() {
        let prefix = [0x77, 0x3c, 0xdc, 0xcd, 0x28, 0xb1];
        let h = handle(&prefix);
        assert_eq!(h.len(), 11);
        assert_eq!(parse_handle(&h), Some(prefix));
        assert_eq!(parse_handle(&h.to_lowercase().replace('1', "l")), Some(prefix));
        assert!(looks_like_handle(&h));
    }

    #[test]
    fn the_bundled_data_matches_android_and_flags_look_alikes() {
        let table = Confusables::bundled();
        assert!(table.len() > 6000);
        // All-Cyrillic "Аӏісе" reads as "Alice"; "Mum" and "mum" read alike.
        assert!(same_reading("\u{410}\u{4cf}\u{456}\u{441}\u{435}", "Alice"));
        assert!(same_reading("Mum", "mum"));
        assert!(!same_reading("Ana", "Bob"));
        assert_eq!(label_conflict("ALICE", ["Bob", "Alice"]), Some("Alice"));
        let lists = NameLists::installed().expect("names-v2 matches the pins");
        assert_eq!((lists.adjectives.len(), lists.nouns.len()), (256, 256));
        let generated = format!("{} {}", lists.adjectives[3], lists.nouns[9]);
        assert!(lists.matches_generated(&generated));
        let mut squashed = generated.replace(' ', "");
        squashed.make_ascii_uppercase();
        assert!(lists.matches_generated(&squashed));
        assert_eq!(normalize(LabelKind::PersonName, &generated), Err("reserved_form"));
    }
    #[test]
    fn stranger_names_match_the_shared_vectors_and_collisions_get_id_suffixes() {
        let file: serde_json::Value = serde_json::from_str(include_str!("../tests/vectors/conformance/labels.json")).unwrap();
        let vector = |id: &str| file["vectors"].as_array().unwrap().iter().find(|v| v["id"] == id).unwrap().clone();
        let hexb = |t: &str| (0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        for id in ["labels-059", "labels-062"] {
            let v = vector(id);
            let rdi = hexb(v["inputs"]["input_rdi"].as_str().unwrap());
            let n = stranger_name(NameInput::Rdi(&rdi)).unwrap();
            assert_eq!(n.name, v["expected"]["name"].as_str().unwrap(), "{id}");
            assert_eq!(u64::from(n.chip), v["expected"]["chip"].as_u64().unwrap(), "{id}");
            assert_eq!(u64::from(n.number), v["expected"]["number"].as_u64().unwrap(), "{id}");
        }
        // labels-063: a throwaway node ID is its own input (digest bytes pinned).
        let v = vector("labels-063");
        let node = hexb(v["inputs"]["input_throwaway_node_id"].as_str().unwrap());
        let d = stranger_digest(&node, 0).unwrap();
        let expected: Vec<u8> = v["expected"]["d_0_3"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as u8).collect();
        assert_eq!(&d[..4], expected.as_slice());
        // labels-064: only 6-byte RDIs and 16-byte throwaway IDs.
        assert!(stranger_name(NameInput::Rdi(&[0; 7])).is_none());
        assert!(stranger_name(NameInput::Rdi(&[0; 16])).is_none());
        assert!(stranger_name(NameInput::Throwaway(&[0; 6])).is_none());
        assert!(stranger_name(NameInput::Throwaway(&[0; 32])).is_none());
        // labels-065 and CF-PRIV-22 suffixes.
        let a = hexb("44e69c0a7a7d");
        let na = stranger_name(NameInput::Rdi(&a)).unwrap();
        assert_eq!(disambiguate(&[(a.clone(), na.clone())]), vec!["mild bream".to_string()]);
        let b = vec![1, 2, 3, 4, 0x12, 0x34];
        let same_number = StrangerName { tail: [0x12, 0x34], ..na.clone() };
        let other_number = StrangerName { number: 7, tail: [0x12, 0x34], ..na.clone() };
        assert_eq!(disambiguate(&[(a.clone(), na.clone()), (b.clone(), other_number)]), vec!["mild bream 19".to_string(), "mild bream 07".to_string()]);
        assert_eq!(
            disambiguate(&[(a.clone(), na.clone()), (b.clone(), same_number)]),
            vec!["mild bream 19-7a7d".to_string(), "mild bream 19-1234".to_string()]
        );
        // The same ID twice (one phone, two addresses) is no collision.
        assert_eq!(disambiguate(&[(a.clone(), na.clone()), (a, na)]), vec!["mild bream".to_string(), "mild bream".to_string()]);
    }

}
