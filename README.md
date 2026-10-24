# Pocketlink

Your phone and your computer, together. Drag a file onto the window and it's on your phone.
See your phone's notifications, reply to messages, ring a lost phone, browse its files.

Pocketlink is a standalone app for the COSMIC desktop. It doesn't need KDE Connect installed:
it has its own background service, written in Rust, that speaks the
[KDE Connect protocol](https://invent.kde.org/network/kdeconnect-meta) (version 8). That means it
already works with the **KDE Connect app for Android and iOS** today, and with the Pocketlink
Android app once that exists.

## Features

| Area | What you get |
| --- | --- |
| Drag & drop | Drop files, links or text anywhere on the window to send them |
| Pairing | Pair from either side, with a code shown on both screens to check |
| Dashboard | Battery ring, charging state, signal strength, connection details |
| Transfers | Live progress, cancel, and *Open* / *Show in folder* for received files |
| Notifications | Phone notifications on your desktop and in the app, with reply and dismiss |
| Clipboard | Send your clipboard to the phone; text from the phone lands on yours |
| Quick actions | Ring phone, browse its files (via `sshfs`), ping |

## How it works

```
┌────────────────────┐          D-Bus           ┌───────────────────────┐
│  pocketlink (app)  │ ◀──────────────────────▶ │  pocketlink-daemon    │
│  Rust + libcosmic  │  io.github.namaray.      │  pairing, plugins,    │
└────────────────────┘  Pocketlink1             │  transfers, desktop   │
                                                │  notifications        │
                                                └──────────┬────────────┘
                                                           │ pocketlink-proto
                              Wi-Fi: UDP 1716 discovery, TLS over TCP
                              Bluetooth: RFCOMM via BlueZ + channel multiplexer
                                                           │
                                                   📱 phone app (KDE Connect
                                                      protocol v8)
```

| Crate | What it is |
| --- | --- |
| `pocketlink-proto` | The protocol: identity packets, certificates, discovery, the TLS handshake (with its flipped roles), pairing codes, payload transfers, and the Bluetooth multiplexer |
| `pocketlink-daemon` | The background service: devices, pairing state, pinned certificates, plugins, transfers, the D-Bus API |
| `pocketlink-core` | The D-Bus client and the shared types apps use |
| `pocketlink` | The COSMIC app |

## Security

Security works like KDE Connect: every device has a self-signed certificate, and pairing pins
it. After pairing, a device claiming the same id with a different certificate is refused, and
file transfers check the certificate on both ends.

- Unpaired devices can only ask to pair; everything else they send is ignored. At most 42
  unpaired devices can be connected at once.
- A paired device is trusted: it can send files (saved to Downloads under a cleaned-up name),
  put text on your clipboard, and open http(s) links in your browser.
- *Browse files* only reacts to the phone's answer to your own request, and the SFTP details in
  it are validated before `sshfs` sees them.
- Text from the phone is escaped before it's shown in desktop notifications.
- Any program running as you can use the D-Bus API, the same as with KDE Connect.
- Over Bluetooth there is no TLS: the link relies on Bluetooth pairing, and the certificate in
  the identity packet is compared with the pinned one but not proven.
- `sshfs` is run with `StrictHostKeyChecking=no`, like KDE Connect, so the phone's SFTP host key
  isn't verified.

## Wi-Fi and Bluetooth

Pocketlink prefers Wi-Fi. When a paired phone is also reachable over Bluetooth, it keeps that
link open too, and carries on over Bluetooth when Wi-Fi goes away (and back when it returns).
Bluetooth is slow (roughly 100–200 KB/s), so it's best for notifications, clipboard, battery and
small files; browsing the phone's files needs Wi-Fi.

To use Bluetooth, pair the phone with this computer in the system Bluetooth settings first, and
turn on Bluetooth in the phone app's settings. There's no TLS over Bluetooth: the link is
protected by Bluetooth pairing, and the device's certificate is still checked against the one
pinned when you paired it in Pocketlink.

## Build and run

```bash
sudo apt install pkg-config libxkbcommon-dev libwayland-dev libexpat1-dev \
  libfontconfig-dev libfreetype-dev libinput-dev libudev-dev libegl-dev cmake just sshfs
just install    # app + daemon into ~/.local, D-Bus activation, autostart
pocketlink
```

During development, `just daemon` runs the service in the foreground with debug logs and
`just run` starts the app.

Pocketlink can run next to KDE Connect (they share UDP port 1716), but your phone will then see
this computer twice. Once you're happy with Pocketlink, turn KDE Connect off.

Data lives in `~/.config/pocketlink` (your identity and paired devices). Received files go to
your Downloads folder.

## Tests

```bash
just test
```

Besides unit tests, this runs two end-to-end tests:

- `pocketlink-proto/tests/loopback.rs`: two devices on loopback discover each other, handshake,
  exchange packets and move a file; a transfer to the wrong certificate is refused.
- `pocketlink-daemon` unit tests include a fake phone over an in-memory Bluetooth link: pairing
  and files both ways through multiplexer channels.
- `pocketlink-daemon/tests/e2e.rs`: starts the real daemon and drives it as a fake phone over
  the network and D-Bus: pairing (checking the code), battery, notifications, replies, ringing,
  files both ways, and a hostile file name.

## Roadmap

- [x] Own protocol implementation and daemon (no KDE Connect needed)
- [x] App: pairing, drag and drop, transfers with progress, notifications with reply
- [x] Bluetooth fallback (RFCOMM + multiplexer), needs testing with a real phone
- [ ] Pocketlink Android app (fork of KDE Connect Android with a new UI)
- [ ] mDNS discovery, for networks that block broadcasts
- [ ] Media controls, saved commands, clipboard history
- [ ] COSMIC panel applet, *Send to phone* in the file manager, global hotkey
- [ ] Sending folders

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Pocketlink is not affiliated with KDE.
