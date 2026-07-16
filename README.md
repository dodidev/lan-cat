# lan-cat

Secure, peer-to-peer clipboard sync for macOS and Linux Wayland desktops.

## Security model

- Noise XX pairing with a code confirmed on both devices.
- Noise KK authenticated encryption for every later connection.
- Peer identity keys are pinned; unknown devices cannot sync.
- Clipboard content is never written to config or logs. Received clipboard files use private
  temporary directories so desktop apps can paste them; these are removed as they expire or the
  daemon exits. Explicit file transfers use hidden `.part` files until acceptance and completion.
- No cloud, account, relay, telemetry, or internet service.

`lan-cat` protects against LAN eavesdropping, tampering, and impersonation. It does not protect
clipboard data from software already running as your local user.

## Platform support

- macOS 13 or newer through `NSPasteboard`.
- Wayland compositors implementing `ext-data-control-v1` or `wlr-data-control-v1`, including
  KDE Plasma, Sway, Hyprland, niri, and similar compositors.
- GNOME/Mutter is unsupported because it does not expose either background data-control protocol.
- X11, copied directories, and SVG/TIFF/PDF rich clipboard formats are not supported. Any of those
  formats can still sync when copied as a regular file.

Linux integration uses Wayland protocols directly through `wl-clipboard-rs`; it prefers the modern
`ext-data-control-v1` protocol and falls back to `wlr-data-control-v1`.

## Build and setup

```sh
cargo build --release
```

Stop the daemon on both devices, then run this command concurrently on both:

```sh
lan-cat pair
```

Compare the six-digit authentication code and confirm it on both terminals. Start sync afterward:

```sh
lan-cat daemon
```

For a user service that remains manual-start:

```sh
lan-cat service install
lan-cat service start
```

Other commands:

```text
lan-cat status
lan-cat peers
lan-cat pause
lan-cat resume
lan-cat unpair <peer-id-or-unique-prefix>
lan-cat name set-name
lan-cat share <file> [more-files...]
lan-cat transfers
lan-cat integration install
```

## Explicit file sharing

`lan-cat share` opens a native GUI without changing the clipboard. Select a connected peer, review
the files, and start the transfer. The receiving device shows an accept/reject dialog with an
editable destination folder. Both sides show byte progress, transfer speed, errors, and cancellation.

Files are sent as 48 KiB encrypted chunks. Every chunk requires an authenticated acknowledgement
before the next chunk is sent. The receiver writes hidden `.part` files and atomically renames each
completed file. Existing destination files are never overwritten. Transfers support up to 256 files
and 100 GiB total; resume after daemon restart is not yet supported.

Copying files normally in Finder or Thunar opens a two-button confirmation window. **Normal copy
only** is selected by default and sends nothing. Use Up/Down or Tab to select, then Enter to
confirm; mouse clicks also work. **Share with lan-cat** continues to peer selection and the
dedicated transfer. This also works with Command/Ctrl+C; lan-cat detects a file clipboard change,
not the specific menu action. Text and image clipboard sync is unchanged.

Install desktop file-manager actions after placing the final binary at its permanent path:

```sh
lan-cat integration install
```

- macOS: installs **Share with lan-cat** as a Finder Quick Action.
- Linux: installs **Share with lan-cat** as a Thunar custom action.

Remove them with `lan-cat integration uninstall`.

## Behavior

- Plain text, HTML, RTF, PNG, and copied regular files are synchronized.
- Up to 64 files can be copied together. Aggregate clipboard payload limit is 16 MiB.
- File names and contents are preserved; permissions, timestamps, extended attributes, and resource
  forks are not.
- Protocol v4 requires all syncing peers to run an upgraded daemon.
- Existing clipboard content is captured at startup and replayed to peers during the same daemon run.
- Latest in-memory event is sent when a peer reconnects during the same daemon run.
- Pause discards events; resume takes a new baseline and does not replay old content.
- Concurrent copies converge using version vectors and device-ID tie-breaking.
- Pair discovery expires after two minutes. Normal discovery remains LAN-local via mDNS.

Allow multicast DNS (UDP 5353) and local TCP traffic in host firewalls. Normal sync ports are
dynamically selected.
