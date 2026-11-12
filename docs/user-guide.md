# MeshLive for Windows: user guide

MeshLive makes private voice and video calls with people nearby over Bluetooth. It needs no internet, no phone
network and no account. This guide covers installing it, the first run, calls and contacts, common problems and how
to remove it.

MeshLive for Windows is in **beta**. Download the tester zip from the
[releases page](https://github.com/MeshLive/meshlive-windows/releases/latest), or build it from source
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

1. Download the zip and `SHA256SUMS` from the [releases
page](https://github.com/MeshLive/meshlive-windows/releases/latest). Check the zip in PowerShell with `Get-FileHash
.\MeshLive-<version>-windows-x64.zip -Algorithm SHA256` and compare the hash with the zip's line in `SHA256SUMS`. If
they differ, delete the zip. 2. Extract it to a folder you keep, for example a `MeshLive` folder in your Documents. 3.
Run `meshlive.exe`. Windows SmartScreen may show "Windows protected your PC" because the zip is not code-signed. If the
hash matched, choose **More info**, then **Run anyway**.

On first run the zip version adds a **MeshLive** entry to your Start menu. Windows needs it to show MeshLive's
notifications (an incoming call while the app is in the background).

The zip and Store versions use the same data folder (`%LOCALAPPDATA%\MeshLive`). Your identity and contacts stay the
same if you switch between them.

## First run

1. Turn on Bluetooth (Windows Settings > Bluetooth & devices).
2. MeshLive asks for an optional name. It is sent, encrypted, only in calls, to the people you choose under
   Settings > **Who sees my name** (people you call or who call you, only your contacts, or nobody).
3. The first call asks for microphone (and, for video, camera) access. Allow MeshLive.

## Using it

- **First start:** four short pages: a welcome (with an optional name for your contacts), access, **Start with
  Windows** (**Allow** or **Not now**) and ready. **Back** or Escape returns a page.
- **Call someone nearby:** Calls > Nearby lists people with MeshLive open. Press **Voice call** or **Video call**.
  With more than eight people nearby, type part of a name in **Search people**.
- **In a call:** the controls are **Mute**, **Video off** (or **Add video**), **End** (in the middle), **Flip**
  (switches between this PC's cameras when it has two) and **Speaker** (chooses where the call plays when this PC has
  more than one output). While muted, hold **Mute** to talk; let go to be muted again. **Ctrl+M** mutes and
  **Ctrl+E** ends.
- **Add a contact:** both of you open **Add contact**, in person. Choose **Show my code** and let their phone scan it,
  then choose **Scan their code** and press **Scan with camera** to read theirs (or paste it). **Save contact as** is
  the name only you see; leave it empty to keep their ID. After that you both see each other as verified contacts, and
  you can call each other by name.
- **A contact's details:** select a contact (or its **More** button) to see how and when you verified them and their
  ID, and to **Rename**, see the **Safety code**, **Block**, **Report** or **Forget** them. A contact checked only by
  safety code shows **Checked by code** in lists (the green check is for contacts scanned in person) and offers
  **Scan to confirm**. When a contact calls themselves by a new name in a call, their details offer **Use it as my
  label** or **Ignore**; your name for them changes only if you choose it.
- **Allow calls:** Settings > **Allow calls** (on by default where this PC supports it). Incoming calls ring in the
  app; the taskbar flashes and a notification appears when MeshLive is in the background. While Allow calls is on,
  closing the window keeps MeshLive in the notification area so phones can still call; choose **Exit** there to quit.
- **Decline:** after you decline a caller who uses a one-time ID, MeshLive offers **Block all unknown callers**.
  **Not now** is remembered.
- **Block someone:** **Block** on a contact or in Recent calls, **Decline and block** on an incoming call, or **Block
  this person** while watching or listening. Settings > Privacy has **Block unknown callers** and **Blocked people**,
  where you can unblock. A call that turns out to be with someone you blocked ends within seconds, before your camera
  or microphone starts. Answering a stranger still shows them who you are, as for any answered call.
- **Report someone:** **Report** on a contact, on an incoming call (it also declines), in the **Block** question, or
  while watching or listening blocks them on this PC. **Tell MeshLive** then drafts an email in your own email app
  with the subject "MeshLive: report a concern"; add what happened and send it yourself. MeshLive sends nothing, and
  the draft holds no names, IDs or codes unless you type them.
- **Start with Windows:** Settings > **Start with Windows** starts MeshLive hidden in the notification area when you
  sign in. It is off by default.
- **Watch or go live (beta):** the **Live** tab lists live streams from phones nearby. Streams this PC cannot watch are
  folded into one line that says what is needed. **Go live** streams this PC's camera or microphone to phones. A video
  broadcast has **Mute**, **Camera off** (or **Camera on**), **End**, **Flip** and **Pause**, with a timer. Audio only
  shows the microphone level, how many are listening and a timer, and can **Switch to video** with the same title. An
  audio broadcast asks "Still broadcasting" after 30 minutes with nobody listening or after 2 hours, and stops by itself
  5 minutes after an unanswered question. If this PC cannot do either, the Live tab says why.
- **Video quality:** Settings > **Video quality** says **Standard**: Go live sends 426 × 240 at 15 fps and 200 kbps
  with VP9. To choose the encoder, resolution, frame rate and bitrate, turn on Settings > **Manual video** > **Manual
  video settings** (until MeshLive closes). **Fill screen with video** (on) fills the video area in calls and while
  watching; **Warn when video freezes** (Experimental, off) dims a stream's picture that stopped while you watch and
  says when it last updated.
- **Status:** the dot at the top right shows whether MeshLive is ready; select it to see your name and whether
  phones can reach this PC.
- **Contact us:** Settings > **Contact us**, or Settings > Help and feedback > **Report a bug**, drafts an email to
  meshlive.dev@gmail.com with only the app version. If no email app opens, MeshLive shows the address to copy.

On an Android phone, check that MeshLive > Settings > Advanced > Connection > **Find iPhones** is on (it is by
default). Despite its name, it also lets the phone find Windows PCs.

## Common problems

- **Nobody is listed.**
  - Check that Bluetooth is on and that the other person has MeshLive open.
  - Move closer. Walls and bodies reduce the range.
- **Phones cannot call this PC.**
  - On Android, check that **Find iPhones** is on (see above). Calls do not need Find PCs.
  - Receiving calls needs Windows 11 version 24H2 or newer and a Bluetooth adapter that supports it. Settings shows
    the reason under Allow calls if this PC cannot receive calls. Calling out works on more PCs.
- **A phone doesn't list this PC's live stream, or this PC can't watch a phone.** Live streams between a phone and a
  PC need the phone's MeshLive > Settings > Advanced > Experiments > **Find PCs** and **PC live streams**. Both are
  off by default on Android.
- **A call this PC places stutters or drops with one phone.** Windows uses slower Bluetooth timing for voice calls
  the PC places, and some phones (an ASUS phone on Android 11 in our tests) cannot keep up with it. Call the PC from
  that phone instead; calls in that direction were not affected.
- **"Bluetooth on this PC stopped responding".** Windows stopped answering MeshLive's Bluetooth requests. Turn
  Bluetooth off and on in Windows settings; if that doesn't help, restart the PC. Until then, calls with some phones
  (Samsung phones in our tests) drop after about 30 seconds, and such a call ends with "Call dropped: Bluetooth on
  this PC stopped responding".
- **The camera doesn't start in Add contact.** Windows asks the first time MeshLive uses the camera. If you said no,
  open Settings > Privacy & security > Camera (the app has an **Open camera settings** button) and allow MeshLive, or
  paste their code instead.
- **No sound or no picture.**
  - Check Windows Settings > Privacy & security > Microphone and Camera, and allow MeshLive.
  - The call screen says when the microphone or camera is blocked.
- **The other person hears an echo.** Use headphones, or lower the speaker volume.
- **A call from a stranger ends with "Couldn't confirm who this is".** After you answer, a caller that is not a
  contact has 10 seconds (more through relay phones) to prove who it is. Until it does, MeshLive sends none of your
  sound or picture. A weak link can delay that proof: move closer and ask them to call again.
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

Use the [bug form](https://github.com/MeshLive/meshlive-windows/issues/new?template=bug_report.yml), the
[PC test report](https://github.com/MeshLive/meshlive-windows/issues/new?template=device-report.yml), or Settings >
**Contact us** (an email draft with only the app version; you review and send it). To report a person, use **Report**
on the contact, the incoming call or the stream, then **Tell MeshLive**. Settings can
export a diagnostics report; it contains only the app version, the Windows build, fixed on/off settings and any
scrubbed crash text. Never include codes, keys or personal names in a report. Security problems go through
[SECURITY.md](../SECURITY.md), never a public issue.
