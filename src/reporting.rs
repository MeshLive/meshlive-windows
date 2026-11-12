//! Contact us, Report a bug and "Tell MeshLive" (Android `ContactEmail`):
//! an email draft to the project's address that the user reviews and sends
//! in their own email app. MeshLive sends nothing itself.
//!
//! Only the fixed draft text and the app version go in. This module takes
//! no name, ID, fingerprint, key, code or contact, so none can reach a
//! draft; a version that is not plain digits and dots is left out.

/// Android `contact_email`.
pub const ADDRESS: &str = "meshlive.dev@gmail.com";

/// Which draft (Android `ContactEmail.open`'s `concern`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Draft {
    /// Contact us (Settings) and Report a bug (Help and feedback).
    Contact,
    /// "Tell MeshLive" after a report (Android `concern = true`).
    Concern,
}

impl Draft {
    /// The subject (Android's two subjects, so the inbox tells them apart).
    pub fn subject(self) -> &'static str {
        match self {
            Draft::Contact => "MeshLive",
            Draft::Concern => "MeshLive: report a concern",
        }
    }
    /// The body: the app version for Contact us; Android's prompt for a
    /// concern. Nothing else.
    pub fn body(self, version: &str) -> String {
        match self {
            Draft::Contact => plain_version(version).map(|v| format!("MeshLive for Windows {v}")).unwrap_or_default(),
            Draft::Concern => "What happened:".to_owned(),
        }
    }
}

/// A version is digits and dots only, at most 24 bytes (as
/// `crash::share_text`); anything else is left out.
fn plain_version(version: &str) -> Option<&str> {
    (!version.is_empty() && version.len() <= 24 && version.bytes().all(|b| b.is_ascii_digit() || b == b'.')).then_some(version)
}

/// Percent-encoding for a `mailto:` field (RFC 6068): every byte except
/// A-Z a-z 0-9 - . _ ~ becomes %XX.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// The `mailto:` link: the address, the subject and the body, nothing else.
pub fn draft_uri(draft: Draft, version: &str) -> String {
    format!("mailto:{ADDRESS}?subject={}&body={}", encode(draft.subject()), encode(&draft.body(version)))
}

/// Hands the draft to `opener` (the shell's `mailto:` handler in the app, a
/// recorder in tests). False: no email app took it.
pub fn open(draft: Draft, version: &str, opener: &mut dyn FnMut(&str) -> bool) -> bool {
    opener(&draft_uri(draft, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Percent-decoding (RFC 3986) for the checks.
    fn decode(text: &str) -> String {
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
    /// The address and the decoded (key, value) fields of a `mailto:` link.
    fn fields(uri: &str) -> (String, Vec<(String, String)>) {
        let rest = uri.strip_prefix("mailto:").expect("a mailto: link");
        let (to, query) = rest.split_once('?').expect("a query");
        let pairs = query
            .split('&')
            .map(|p| {
                let (k, v) = p.split_once('=').expect("key=value");
                (decode(k), decode(v))
            })
            .collect();
        (to.to_owned(), pairs)
    }

    /// Android `ContactEmailTest`: each draft is the one address with exactly
    /// a subject and a body, every special character encoded.
    #[test]
    fn each_draft_is_the_address_a_subject_and_a_body_only() {
        for draft in [Draft::Contact, Draft::Concern] {
            let uri = draft_uri(draft, "0.2.2");
            let (to, pairs) = fields(&uri);
            assert_eq!(to, ADDRESS);
            assert_eq!(pairs, vec![("subject".to_owned(), draft.subject().to_owned()), ("body".to_owned(), draft.body("0.2.2"))]);
            assert!(!uri.contains('#') && !uri.contains(' ') && !uri.contains('\n'), "{uri}: everything is encoded");
        }
        assert_ne!(Draft::Contact.subject(), Draft::Concern.subject(), "a concern is told apart in the inbox");
        assert!(Draft::Contact.body("0.2.2").contains("0.2.2"), "Contact us says which version");
        assert!(!Draft::Concern.body("0.2.2").contains("0.2.2"), "a concern carries nothing but the prompt");
    }

    /// Whatever is passed as the version, only plain digits and dots reach
    /// the draft: a name, an ID or a line break never does.
    #[test]
    fn a_version_that_is_not_plain_digits_never_reaches_the_draft() {
        for odd in ["0.2.2-Asha", "7KQ2M-4ZD80", "", "0.2.2\nAsha"] {
            let uri = draft_uri(Draft::Contact, odd);
            let plain = decode(&uri);
            assert!(!plain.contains("Asha") && !plain.contains("7KQ2M"), "{odd:?} -> {plain}");
            assert_eq!(fields(&uri).1[1].1, "", "{odd:?}: the body is left empty");
        }
    }

    /// The opener gets the draft and nothing else, and its answer is passed on.
    #[test]
    fn open_hands_only_the_draft_to_the_opener_and_says_whether_it_opened() {
        for answer in [true, false] {
            let mut seen = Vec::new();
            let opened = open(Draft::Concern, "0.2.2", &mut |uri: &str| {
                seen.push(uri.to_owned());
                answer
            });
            assert_eq!(opened, answer);
            assert_eq!(seen, vec![draft_uri(Draft::Concern, "0.2.2")]);
        }
    }
}
