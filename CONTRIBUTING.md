# Contributing to MeshLive for Windows

For anyone who wants to report a bug, share a PC result, improve the docs or send code. You do not need to write code
to help: a clear bug report or a result from your PC and Bluetooth adapter is a real contribution.

By taking part you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). If you found a security problem, do not open
a public issue; use [SECURITY.md](SECURITY.md).

Which document answers what is listed under [Documentation in the README](README.md#documentation).

## Report a bug or a PC result

Use the forms behind **New issue** ([templates](.github/ISSUE_TEMPLATE/)):

- **Bug report**: something failed (calls, video, audio, crash, adding a contact). Attach the report from
  Settings > **Export diagnostics**. It holds no media, traces, contact names, Bluetooth addresses or keys.
- **PC test report**: what worked on which PC, Bluetooth adapter and phone, including when everything worked.
- **Feature request**: describe your situation (who, where, how many devices), not only the feature.

Name the PC (model), Windows version and build, the Bluetooth adapter, the phone (model and Android version), the app
version or commit, what you expected, what happened and how often. Do not attach photos, videos, recordings or
screenshots of other people.

## Build and test

Requirements, build and test commands, and packaging are in [building.md](docs/dev/building.md); how to check the app
against your own phone is in [testing.md](docs/dev/testing.md).

Crates are pinned in `Cargo.toml` and `Cargo.lock`, and native sources and build tools by SHA-256 in
`sources.lock.json`. If you change a dependency, update the lock files and `THIRD_PARTY_NOTICES.md` together. The
application identity is `io.github.meshlive.app`; do not change it in a feature branch.

## Send a pull request

For anything bigger than a typo, open an issue first so the approach is agreed before the code exists.

- **Reproduce first** with a failing test, a log or a measurement. **Fix narrowly**, at the root cause, and add a
  focused regression that fails without the fix.
- **Measure on real devices** when you touch Bluetooth, codecs, audio or lifecycle: one build, the same settings
  before and after, and the PC, adapter and phones named. Never silently change a requested bitrate, frame rate,
  resolution, encoder, audio setting or buffer to make a test pass.
- **Every PC and phone meets the same gates** ([testing.md](docs/dev/testing.md#acceptance-gates)). Keep the safety
  guards: authentication, ordered encryption, hop-local control, admission and overload protection, the adapter
  privacy allowlist. If you think a limit is too low, show measured evidence.
- **Do not call something passed without evidence.** Unmeasured stays unmeasured; a fallback is not a pass for the
  requested path.
- **Experiments ship default off**, labelled Experimental, with a row in the [switch register](docs/dev/switches.md).
  The losing arm of an A/B stays switchable.
- **Adapter and platform policies** (a Bluetooth adapter family, a Windows build, a codec driver) need a matcher,
  official sources (Microsoft Learn, Bluetooth SIG specifications, vendor documentation), measured evidence, a
  confidence level and an off switch, like the entries in `src/qw_policy.rs`.
- **Protocol or security changes** (wire format, identity, encryption, admission) start in
  [meshlive-android](https://github.com/MeshLive/meshlive-android): it owns the
  [protocol reference](https://github.com/MeshLive/meshlive-android/blob/main/docs/dev/protocol/README.md), and every
  MeshLive app must stay byte-compatible. Open the design there first, get it reviewed, then
  implement it here behind a switch. Do not post exploit details publicly.
- **Docs**: keep each fact in one document and replace outdated text instead of appending. Explain a changed
  assumption in plain language: what was wrong, what changed, why, and the evidence.
- **Platform**: Windows behaviour can differ from Android's where that is the best design for a PC; the wire format,
  security rules and button styles stay the same as Android's.

## Using AI tools

AI-assisted contributions are welcome and go through the same review as any other change.

- You are responsible for every line. Read, understand and run what the tool wrote.
- Say in the pull request when AI did a substantial part (a sentence: which tool, for what).
- An agent's claim that it "tested" something is not evidence until you see the logs or numbers. Re-read its
  documentation against the evidence; agents overclaim.
- Check licences: do not submit code you cannot license under GPL-3.0-or-later, and keep attribution for adapted code.
- Keep secrets and private data out of prompts (see the privacy rules below). Give the tool only the files and devices
  it needs, and review any cleanup it proposes before it deletes anything.
- Pull requests with no sign of human review, or whose author cannot explain the change, may be closed.

## Licensing and sign-off

MeshLive is **GPL-3.0-or-later** ([LICENSE](LICENSE)); contributions are accepted under the same licence and you keep
your copyright. Copyright lines read "The MeshLive Authors" ([AUTHORS](AUTHORS)).

- Sign off every commit with `git commit -s`; this certifies the
  [Developer Certificate of Origin 1.1](https://developercertificate.org/).
- Do not copy code, assets, test media or protocol text from another project unless its licence is compatible. Keep
  the original notice and add the component to [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
- Test media must be generated or licensed for redistribution; never commit footage or recordings of real people.

## Privacy rules

The repository is public and permanent. Never commit, paste into an issue or put in a fixture: device serial numbers,
Bluetooth or Wi-Fi addresses, Windows device instance IDs, real node IDs, rotating IDs or key fingerprints; private
photos, recordings or screenshots of other people; personal paths, usernames, computer names, emails or phone numbers;
signing certificates, keys or passwords. Use placeholders such as `AA:BB:CC:00:00:01`. Logs and traces stay out of
Git; share summaries (numbers, models, short commit hashes) instead.

## Pull request checklist

- [ ] The problem is described, with evidence that it is real.
- [ ] The change is narrow and a focused regression covers it.
- [ ] `scripts\Build.ps1 -Action Test -Configuration Release` passes.
- [ ] Bluetooth, media or lifecycle changes name the PC, adapter, phones and the same-settings comparison.
- [ ] New behaviour is default off, labelled Experimental and registered in [switches.md](docs/dev/switches.md).
- [ ] Protocol or security changes link to a reviewed design in meshlive-android.
- [ ] The documents that own the changed behaviour are updated.
- [ ] Commits are signed off, the diff contains no private data, and substantial AI assistance is disclosed.
