# MeshLive for Windows: privacy

MeshLive lets you call people near you over Bluetooth. It is built so that nobody, including the people who make
MeshLive, can collect your calls or data. This page says what the Windows app stores, what it sends over Bluetooth,
and the limits you should know about.

## What MeshLive does not do

- It has no servers and no account. It does not use the internet.
- It does not collect, upload, sell or share personal data. There are no analytics, advertising, tracking or crash
  uploads.
- It never records calls or streams.

## What stays on your PC

These are stored only on your PC, under your Windows user's app data (`%LOCALAPPDATA%\MeshLive`):

- your MeshLive identity key, a non-exportable P-256 signing key in Windows' key store (CNG), which other devices
  use to recognise this PC;
- the contacts you verified in person (their key fingerprints and the names you gave them), sealed with Windows data
  protection (DPAPI);
- short-lived presence keys that let your contacts recognise you nearby (DPAPI-sealed);
- people you blocked, your settings, and the name you chose;
- crash reports and crash dumps, if the app crashed: at most a few small text reports and three minidumps. They never
  leave the PC unless you send the text yourself (see "Crash reports" below).

**Settings > Delete everything** deletes all of it, including the identity key. Uninstalling MeshLive also removes it.

## What goes over Bluetooth

- **While MeshLive can be called or is searching:** a short Bluetooth advertisement with an ID that changes every
  15 minutes. It contains no name. Devices nearby can see that a MeshLive device is present.
- **When you call someone or they call you:** your voice and, in a video call, your camera picture. They are encrypted
  end to end and go only to the other device.
- **Your name:** if you set one, it is sent only to verified contacts during a call. It is signed, so it cannot be
  changed on the way.
- **Your key:** a verified contact learns your identity key when you add each other. A stranger you call meets a
  temporary key, and learns your real one only if the call is answered. When a stranger calls this PC, answering
  shows them your real key, and they must show theirs within 10 seconds (up to 40 seconds through relay phones).
  Until they do, the PC sends none of your sound or picture and drops theirs; a caller that does not is hung up.

## Microphone and camera

- The microphone is used only during a call (and, in the experimental Go live, while someone is listening).
- The camera is used only for your video in a video call, to scan a contact's code, or in the experimental Go live.
- Both follow your Windows privacy settings, and the app shows when either is on. Muting discards audio that was
  captured but not yet sent.

## Crash reports and diagnostics

- If MeshLive crashes, it saves a short text report and a minidump on the PC. On the next start it offers to share
  the text report: **Share** opens your own email app with the text (app version, Windows build, crash kind and code
  offsets; names, keys and codes are scrubbed). You choose whether and to whom to send it. Minidumps are never shared
  or exported.
- **Settings > Export diagnostics** saves a text report you can attach to a bug report. It contains the app version,
  the Windows build, fixed on/off settings and any scrubbed crash text, and no traces, device names or addresses.

## Known limits

We list these so you can judge the risk for your own situation.

- **Bluetooth metadata is visible.** Anyone in radio range can see advertisements, connection timing, and the size and
  timing of encrypted traffic. Encryption protects content, not the fact that you are communicating.
- **Windows chooses the advertising address.** Windows does not let an app read or set the address it advertises
  from, so MeshLive only announces itself (to receive calls) on Bluetooth adapters whose address rotation has been
  measured; see [Supported PCs](devices.md#bluetooth-adapters). Other PCs can still call out.
- **Phones paired with this PC.** If Windows is paired (bonded) with a phone, that phone's Bluetooth stack recognises
  the PC whenever it connects, even though MeshLive itself talks as a stranger. The call screen says so; remove the
  pairing in Windows Bluetooth settings to call privately. MeshLive never pairs, unpairs or changes pairings.
- **Going live can expose the PC's address.** The experimental Go live connects to viewers from the PC's Bluetooth
  address, which may be its fixed public address. The app asks for your consent each time you go live.
- **Identity is not a person.** A key does not prove who owns a device. Compare codes in person for contacts you care
  about.
- **Delete everything has limits.** It cannot reach copies outside the app, Windows system logs or backups, and it
  cannot promise erasure of flash storage.
- **No independent security audit** of the protocol or this app yet.

The protocol's full security rules are in
[meshlive-android's protocol reference](https://github.com/MeshLive/meshlive-android/blob/main/docs/dev/protocol/README.md).

## Children

MeshLive is not directed at children and collects no data from anyone.

## Changes and contact

Changes to this policy are published with the app's release notes. Questions: open an issue on this repository, or
write to meshlive.dev@gmail.com. Security problems: see [SECURITY.md](../SECURITY.md).
