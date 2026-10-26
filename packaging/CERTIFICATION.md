# MeshLive for Windows: Store certification notes (draft)

Notes for the Microsoft Store submission ("Notes for certification") and for reviewers. Draft until the Store name is reserved and the package is accepted.

## What the app does

MeshLive makes private voice and video calls with people nearby over Bluetooth Low Energy. It needs no internet connection, no phone network and no account. Calls are end-to-end encrypted between the two devices. Contacts are added in person by scanning each other's code. The app has no servers and sends nothing to the internet.

## Capabilities and why each is needed

| Capability | Why |
| --- | --- |
| `runFullTrust` | MeshLive is a native desktop app (Rust, Win32 window with Direct3D 11). It uses WASAPI for call audio, Media Foundation for the camera and video codecs, and the Windows Bluetooth LE APIs as a GATT client and GATT server. It runs at the user's integrity level, never asks for elevation, installs no driver or service, and opens no network listener. |
| `bluetooth` | Finding people nearby (Bluetooth LE scanning), calling them (GATT client) and receiving calls (GATT server with a connectable advertisement). Bluetooth is the only transport the app uses. |
| `microphone` | The user's own voice in a call. The microphone is opened only while a call's media runs and closed when the call ends; the Windows privacy setting is respected and its state is shown in the call. |
| `webcam` | The user's own video in a video call, and scanning a contact's code with the camera when adding a contact. The camera is opened only for those two moments and closed afterwards. As a packaged app MeshLive has its own entry in Settings > Privacy & security > Camera: the first use asks the user, and a denial is shown in the app with an "Open camera settings" button (the code can still be pasted). |

No other capability is declared (no internet, no location, no contacts, no broad file access).

## Reviewer path

Testing a call needs a second device with MeshLive (Android, free on the project's releases) within Bluetooth range.

1. Start MeshLive. Settings > Profile > Your name: set a name.
2. Calls > Nearby lists phones running MeshLive nearby. Press Video call or Voice call.
3. On the phone, accept. Both sides see and hear each other; End call ends it.
4. Contacts > Add contact shows this PC's code; the phone scans it, and the PC enters (or scans) the phone's code. Both sides then show each other as verified contacts.
5. Settings > Delete everything deletes this PC's identity, contacts and keys after typing DELETE.

Without a second device the app still starts, shows its own code and settings, and explains that nobody is nearby.

## Privacy

- No data leaves the device except over the Bluetooth link to the person being called or verified, encrypted end to end.
- Nothing is collected, uploaded, sold or shared; there are no analytics, ads or crash uploads.
- Local data (identity key, verified contacts, presence keys, settings, name) is stored in the user's app data, sealed with Windows DPAPI, and deleted by Wipe or by uninstalling.
- Privacy policy: [docs/privacy.md](../docs/privacy.md), published on the MeshLive website before submission.

## Open items before submission

- Reserve the Store name; put the Partner Center identity into `packaging/identity.json`.
- Publish the privacy policy and support page at public URLs.
- Re-check the Bluetooth GATT server and advertisement under the packaged identity (the on-air checks in [testing](../docs/dev/testing.md)) and a phone-to-PC call.
- Run the Windows App Certification Kit on the built package and keep its report with the candidate.
- Decide the publisher account and the publisher display name (no personal name is used anywhere in the package).
