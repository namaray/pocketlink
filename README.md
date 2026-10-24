# Pocketlink

A friendlier desktop app for connecting your phone to your PC, built for the COSMIC desktop.

Pocketlink is a new UI on top of [KDE Connect](https://kdeconnect.kde.org/). It doesn't replace it.
The KDE Connect background service (`kdeconnectd`) still handles pairing, encryption and the
network protocol. Pocketlink talks to that service over D-Bus, so the KDE Connect Android/iOS app
on your phone keeps working and nothing has to be paired again.

## Why

The stock KDE Connect app is mostly lists of text, and sending a file means picking it from a file
chooser. Pocketlink aims for:

- **Drag and drop everywhere.** Drop files, folders, text or links onto a device card, the panel
  applet or the app window to send them.
- **A visual, glanceable UI** instead of menus of text.

## Planned features (v1)

| Area | What you get |
| --- | --- |
| Drag & drop sending | Drop onto the window, a device card, or the panel applet. Multiple files at once. |
| Device dashboard | Battery ring, charging state, signal strength, *Ring my phone*, media controls with album art, saved-command tiles |
| Transfers | Live transfer list with progress and cancel, plus a gallery of received files with *Open* and *Show in folder* |
| Notifications | Phone notifications as cards, with reply and dismiss |
| Clipboard | Two-way clipboard history |
| COSMIC panel applet | Shows battery at a glance, accepts dropped files, opens quick actions |
| Desktop integration | *Send to phone* in the file manager's right-click menu, and a global hotkey to send the clipboard |

## Architecture

```
┌──────────────────────────┐   ┌──────────────────────────┐
│  pocketlink (app)        │   │  pocketlink-applet       │
│  Rust + libcosmic        │   │  COSMIC panel applet     │
└────────────┬─────────────┘   └────────────┬─────────────┘
             └──────────────┬───────────────┘
                 ┌──────────┴───────────┐
                 │  pocketlink-core     │  zbus bindings + state
                 └──────────┬───────────┘
                            │ D-Bus (session bus)
                 ┌──────────┴───────────┐
                 │  kdeconnectd         │  unchanged, from your distro
                 └──────────┬───────────┘
                            │ LAN / Bluetooth
                     📱 KDE Connect app on your phone
```

D-Bus objects used, all under `/modules/kdeconnect/devices/<id>/`: `share` (`shareUrls`,
`shareText`), `battery`, `connectivity_report`, `clipboard`, `notifications`, `mprisremote`,
`findmyphone`, `sftp`, `sms`, `remotecommands`.

## Requirements

- KDE Connect ≥ 23.08 (`sudo apt install kdeconnect`) with a paired phone
- COSMIC desktop (the main app may work on other desktops too)
- Rust toolchain (`rustup`) to build

## Status

Early planning. Nothing is usable yet.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Pocketlink is not affiliated with KDE.
