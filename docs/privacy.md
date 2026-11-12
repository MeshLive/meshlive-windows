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
  use to recognise this PC (with the experimental `MESHLIVE_TPM_IDENTITY=1`, a new identity key is kept in this PC's
  security chip instead, and cannot be copied off the PC);
- the contacts you verified in person (their key fingerprints and the names you gave them), sealed with Windows data
  protection (DPAPI);
- the name each verified contact calls themselves in calls with you, and whether you scanned each other's codes
  (shown in that contact's details; it never replaces the name you gave them);
- short-lived presence keys that let your contacts recognise you nearby (DPAPI-sealed);
- people you blocked (and when), your settings, and the name you chose;
- crash reports and crash dumps, if the app crashed: at most a few small text reports and three minidumps. They never
  leave the PC unless you send the text yourself (see "Crash reports" below).

**Settings > Delete everything** deletes all of it, including the identity key. Uninstalling MeshLive also removes it.

## What goes over Bluetooth

- **While MeshLive can be called or is searching:** a short Bluetooth advertisement with an ID that changes every
  15 minutes. It contains no name. Devices nearby can see that a MeshLive device is present.
- **When you call someone or they call you:** your voice and, in a video call, your camera picture. They are encrypted
  end to end and go only to the other device.
- **Your name:** if you set one, it is sent, encrypted, only in calls, to the people you choose under Settings > **Who
  sees my name**: **People I call or who call me** (the default), **Only my contacts**, or **Nobody**. With the default,
  someone you call who has not added you gets it only once they answer and prove who they are, a verified contact who
  calls you gets it while it rings, and anyone else who calls you gets it when you answer. It is signed, so it cannot be
  changed on the way. Everyone your choice leaves out sees a generated name. If MeshLive cannot read your saved choice,
  it sends your name to nobody until you choose again.
- **Someone you blocked:** if a call turns out to be with someone you blocked (once they prove who they are),
  MeshLive ends it within seconds with an ordinary hang-up and sends them nothing more: no camera, no microphone, no
  name. Answering a stranger shows them who you are, as for any answered call.
- **Your key:** a verified contact learns your identity key when you add each other. A stranger you call meets a
  temporary key, and learns your real one only if the call is answered. When a stranger calls this PC, answering
  shows them your real key, and they must show theirs within 10 seconds (up to 40 seconds through relay phones).
  Until they do, the PC sends none of your sound or picture and drops theirs; a caller that does not is hung up.

While Add contact is open, MeshLive asks Windows to leave its window out of screenshots, screen recordings and screen
sharing, so your code is not captured. Your code is not printed on screen: **Copy code** puts it on the clipboard, keeps
it out of clipboard history and cloud clipboard, and MeshLive removes it after 60 seconds (or when it closes) unless
you copied something else since. If MeshLive is ended by force (Task Manager, a crash), the code stays on the clipboard
until you copy something else.

## Microphone and camera

- The microphone is used only during a call (and, when you go live, while someone is listening).
- The camera is used only for your video in a video call, to scan a contact's code, or when you go live.
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
- **A phone this PC connects to can learn its Bluetooth address and computer name.** When this PC calls a phone, Windows
  connects from the PC's fixed Bluetooth address (on the PC we tested), and it answers the phone's standard Bluetooth
  request for a device name with this PC's computer name (Windows Settings > System > About). Some phones keep both in
  their Bluetooth logs. MeshLive cannot change either; if the computer name identifies you, rename the PC. Settings >
  Privacy > **Computer name** shows the name phones see (when Windows can read it).
- **Going live can expose the PC's address.** Go live connects to viewers from the PC's Bluetooth
  address, which may be its fixed public address. The app asks for your consent each time you go live.
- **Identity is not a person.** A key does not prove who owns a device. Compare codes in person for contacts you care
  about.
- **Delete everything has limits.** It cannot reach copies outside the app, Windows system logs or backups, and it
  cannot promise erasure of flash storage.
- **No independent security audit** of the protocol or this app yet.

The protocol's full security rules are in [meshlive-android's protocol
reference](https://github.com/MeshLive/meshlive-android/blob/main/docs/dev/protocol/README.md).

## Children

MeshLive is not directed at children and collects no data from anyone.

## Changes and contact

Changes to this policy are published with the app's release notes. Questions: open an issue on this repository, or
use Settings > **Contact us**, which drafts an email to meshlive.dev@gmail.com in your own email app. A draft holds only
the app version (a report of a concern holds only "What happened:"); you review and send it, and MeshLive itself sends
nothing. Security problems: see [SECURITY.md](../SECURITY.md).
