# Security policy

MeshLive is used where the internet may be down or watched, so a security flaw can matter more than in an ordinary
app. We take reports seriously, and we would rather hear about a problem privately and fix it than read about it in
a public issue first.

## Supported versions

MeshLive for Windows is pre-release software. Security fixes go to:

| Version | Supported |
| --- | --- |
| The latest published release | Yes |
| `main` (development) | Yes |
| Older releases | No. Please update to the latest release |

Until the first release, only `main` exists.

## Reporting a vulnerability

**Please do not open a public issue, pull request or discussion for a security problem.**

Report it privately through GitHub Security Advisories:

1. Open the repository's **Security** tab and choose **Report a vulnerability**, or go straight to
   <https://github.com/MeshLive/meshlive-windows/security/advisories/new>.
2. Describe the problem as described below and submit. Only the maintainers can see the report.

If you cannot use GitHub Security Advisories, open a public issue that says only "security report" with no details,
and a maintainer will arrange a private channel. Do not include the technical details, keys or proof of concept in
that issue.

A flaw in the protocol itself (it affects every MeshLive app) can be reported here or in
[meshlive-android](https://github.com/MeshLive/meshlive-android/security/advisories/new); we will coordinate.

### What to include

- Which part is affected (see "Scope" below) and which version or commit.
- What an attacker needs (Bluetooth range, a paired device, a malicious relay, local access to the PC, and so on) and
  what they gain (read content, impersonate someone, make a call or stream misbehave, recover wiped data).
- Steps to reproduce, or a short proof of concept. The diagnostics report (Settings > Export diagnostics) contains no
  media, traces, contact names, addresses or keys.
- The Windows version, Bluetooth adapter and phones you tested with.
- Whether you plan to publish your findings, and any date you would like us to work towards.

Please do not include real private media, other people's data or your own keys. Please do not attack other people's
devices, and test with devices you own.

## What to expect

This is a volunteer project, so response times are best effort and there is no service-level promise and no bounty.
We aim to:

- acknowledge a report within about a week,
- tell you whether we can reproduce it and how we rate it,
- fix confirmed problems as soon as we reasonably can, then publish an advisory and credit you if you wish,
- agree a disclosure date with you. Without a better reason we prefer to publish once a fix is released, and no later
  than 90 days after the report.

If we decide something is not a vulnerability, we will explain why.

## Scope

In scope:

- **Protocol, identity and encryption** as implemented here: the BMH2 and BMH3 handshakes, BMS1 hop and BME1 endpoint
  records, key agreement, signatures, replay and ordering protection, identity verification and verification codes.
- **Calls**, including relayed calls, and the audio and video carried inside them.
- **Live streams** (experimental): admission, source binding, signed audio-only records, forwarded-stream grants.
- **Discovery privacy**: anything that lets a nearby listener link this PC's rotating IDs or learn its identity beyond
  the stated limits, including the adapter allowlist in `src/qw_policy.rs`.
- **Local storage**: the CNG identity key, DPAPI-sealed stores, crash reports, diagnostics export.
- **Delete everything**: cases where a completed wipe leaves keys or app data that the design says it removes.
- **Build and supply chain** in this repository: `Cargo.lock`, `sources.lock.json` and the pinned native sources,
  the vendored renderer, packaging scripts and workflows.

Out of scope, with examples:

- Attacks that need administrator rights, a debugger, malware already running as the user, or physical access to an
  unlocked PC.
- Anything that needs the `test-hooks` build or a test-only switch, unless it can reach the release app.
- Bluetooth radio facts that the project documents: a nearby listener sees that Bluetooth traffic exists, its timing
  and its size.
- Denial of service by radio jamming, or by a nearby device that connects and refuses to forward.
- Findings in third-party components that have no effect on MeshLive (report those upstream).
- Reports from automated scanners without a demonstrated impact.
- Social engineering of maintainers, and spam or abuse in the issue tracker.

If you are unsure whether something is in scope, report it privately anyway.

## Known limitations

We list these so you can judge the risk for your own situation. They are open work, not secrets.

- **No independent security audit.** The design uses standard primitives (P-256 ECDH and ECDSA, HKDF-SHA-256,
  AES-256-GCM, through Windows CNG), but neither the protocol nor this implementation has had a third-party review.
- **Bluetooth metadata is visible.** Anyone in radio range can see advertisements, connection timing, and the size and
  timing of encrypted records. Encryption protects content, not the fact that you are communicating.
- **Windows controls the advertising address.** An app cannot read or set it, so address privacy depends on the
  adapter and driver. The app announces only on measured adapter families; see [docs/devices.md](docs/devices.md).
- **Paired devices recognise each other.** A phone that Windows has paired with recognises the PC at the Bluetooth
  level ([privacy](docs/privacy.md#known-limits)).
- **Identity is not a person.** Compare verification codes in person for contacts you care about.
- **Experimental features are experimental.** Anything labelled Experimental in
  [switches.md](docs/dev/switches.md) is off by default and has had less review.
- **Delete everything has stated limits**: it cannot reach copies outside the app, Windows logs or backups, and cannot
  promise erasure of flash storage.
- **No signed release yet.** Until a signed Store package or a signed release exists, build from source and verify
  what you run.

Thank you for helping keep MeshLive users safe.
