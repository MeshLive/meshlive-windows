# MeshLive for Windows: user guide

MeshLive makes private voice and video calls with people nearby over Bluetooth. It needs no internet, no phone
network and no account. This guide covers installing it, the first run, calls and contacts, common problems and how
to remove it.

MeshLive for Windows is in **beta** and has no published download yet. Until it does, build it from source
([building.md](dev/building.md)).

## Requirements

- Windows 11, 64-bit. Receiving calls needs Windows 11 version 24H2 or newer. Windows 10 (version 2004 or newer) may
  work for calling out, but has not been tested.
- Bluetooth Low Energy (almost every laptop since 2016; a USB Bluetooth adapter also works). Receiving calls also needs
  an adapter whose private addresses have been checked; see [Supported PCs](devices.md).
- For video: a camera. For calls: a microphone and speakers, or a headset.
- Another device with MeshLive within about 10 to 30 metres.

## Install

### Microsoft Store (planned)

A Store listing is planned. The Store will install MeshLive, keep it up to date and sign it, so Windows trusts it.

### Release zip (testers)

When a tester zip is published:

1. Download the zip from the project's release page and check its SHA-256 against `SHA256SUMS` on that page.
2. Extract it to a folder you keep, for example a `MeshLive` folder in your Documents.
3. Run `meshlive.exe`. Windows SmartScreen may show "Windows protected your PC" because the zip is not code-signed.
   If the hash matched, choose **More info**, then **Run anyway**.

On first run the zip version adds a **MeshLive** entry to your Start menu. Windows needs it to show MeshLive's
notifications (an incoming call while the app is in the background).

The zip and Store versions use the same data folder (`%LOCALAPPDATA%\MeshLive`). Your identity and contacts stay the
same if you switch between them.

## First run

1. Turn on Bluetooth (Windows Settings > Bluetooth & devices).
2. MeshLive asks for an optional name. It is sent only to verified contacts during calls.
3. The first call asks for microphone (and, for video, camera) access. Allow MeshLive.

## Using it

- **Call someone nearby:** Calls > Nearby lists people with MeshLive open. Press **Voice call** or **Video call**.
- **Add a contact:** both of you open **Add contact**. Scan each other's code, in person: point their phone at the
  code on your screen, and press **Scan with camera** to read theirs (or paste it). After that you both see each
  other as verified contacts, and you can call each other by name.
- **Receive calls:** Settings > Calls > **Receive calls** (on by default where this PC supports it). Incoming calls
  ring in the app; the taskbar flashes and a notification appears when MeshLive is in the background. While Receive
  calls is on, closing the window keeps MeshLive in the notification area so phones can still call; choose **Exit**
  there to quit.
- **Block someone:** **Block** on a contact or in Recent calls. **Block unknown callers** is in Settings.
- **Start with Windows:** Settings > **Start with Windows** starts MeshLive hidden in the notification area when you
  sign in. It is off by default.

On an Android phone, turn on MeshLive > Settings > Connection > **Find iPhones**. Despite its name, this also lets
the phone find Windows PCs.

## Common problems

- **Nobody is listed.**
  - Check that Bluetooth is on and that the other person has MeshLive open.
  - Move closer. Walls and bodies reduce the range.
- **Phones cannot call this PC.**
  - On Android, turn on **Find iPhones** (see above). The default Windows call announcement does not need Find PCs.
  - Receiving calls needs Windows 11 version 24H2 or newer and a Bluetooth adapter that supports it. Settings > Calls
    shows the reason if this PC cannot receive calls. Calling out works on more PCs.
- **The camera doesn't start in Add contact.** Windows asks the first time MeshLive uses the camera. If you said no,
  open Settings > Privacy & security > Camera (the app has an **Open camera settings** button) and allow MeshLive, or
  paste their code instead.
- **No sound or no picture.**
  - Check Windows Settings > Privacy & security > Microphone and Camera, and allow MeshLive.
  - The call screen says when the microphone or camera is blocked.
- **The other person hears an echo.** Use headphones, or lower the speaker volume.
- **"More than one phone is using this ID".** Another device may be copying an ID. The call still checks the other
  device before it shows who answered.
- **The call screen says this PC is paired with the phone.** Windows paired with that phone earlier, so the phone's
  Bluetooth stack recognises the PC. Remove the pairing in Windows Bluetooth settings to call privately. MeshLive
  never pairs by itself.

## Remove MeshLive and your data

- **Settings > Delete everything** deletes your identity, contacts, keys, name and settings from this PC (type DELETE
  to confirm).
- Then uninstall: Windows Settings > Apps > MeshLive > Uninstall (Store), or delete the extracted folder and the
  **MeshLive** Start-menu entry (`%APPDATA%\Microsoft\Windows\Start Menu\Programs\MeshLive.lnk`) (zip).

## Privacy

There are no servers, accounts, analytics or ads. Calls are encrypted end to end between the two devices. Full
policy: [privacy.md](privacy.md).

## Reporting a problem

Use the [bug form](https://github.com/MeshLive/meshlive-windows/issues/new?template=bug_report.yml) or the
[PC test report](https://github.com/MeshLive/meshlive-windows/issues/new?template=device-report.yml). Settings can
export a diagnostics report; it contains only the app version, the Windows build, fixed on/off settings and any
scrubbed crash text. Never include codes, keys or personal names in a report. Security problems go through
[SECURITY.md](../SECURITY.md), never a public issue.
