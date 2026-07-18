# lan-cat

Secure, peer-to-peer clipboard sync and LAN file sharing for macOS and Linux Wayland desktops.

## Current feature set

- Peer-to-peer clipboard sync for text, HTML, RTF, PNG, and copied regular files.
- Explicit LAN file sharing with peer selection, accept/reject, destination selection, progress,
  transfer speed, cancellation, and safe `.part` writes.
- Local file-copy confirmation popup for Finder/Thunar copy operations: **Normal copy only** by
  default, or **Sync clipboard** when the copied files should be sent through clipboard sync.
- Finder Quick Action on macOS and Thunar custom action on Linux for **Share with lan-cat**.
- Manual-start user service support through `launchctl` on macOS and `systemd --user` on Linux.
- LAN-only discovery through mDNS, with encrypted authenticated connections between trusted peers.
- Opt-in macOS and Wayland cursor sharing on all four screen edges, including click, scroll, and drag.

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

## Install

```sh
cargo build --release
```

Recommended macOS install path:

```sh
sudo install -m 755 target/release/lan-cat /usr/local/bin/lan-cat
```

`/opt/homebrew/bin/lan-cat` is also fine on Apple Silicon if that is your normal user binary path.
Use one stable path before installing services or Finder integration, because those files record the
current executable path.

Recommended Linux user install path:

```sh
install -Dm755 target/release/lan-cat ~/.local/bin/lan-cat
```

System-wide Linux install is also supported:

```sh
sudo install -m 755 target/release/lan-cat /usr/local/bin/lan-cat
```

## Pair and run

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
lan-cat cursor enable
lan-cat cursor disable
lan-cat cursor status
lan-cat integration install
lan-cat integration uninstall
lan-cat service status
lan-cat service stop
lan-cat service uninstall
```

## Configuration and keys

`lan-cat` stores identity keys and trusted peers in `config.json`.

Default macOS path:

```text
~/Library/Application Support/org.lan-cat.lan-cat/config.json
```

Default Linux path:

```text
~/.config/lan-cat/config.json
```

The config contains the local X25519 private/public key pair, device name, pause state, version
clock, and pinned peer public keys. Clipboard contents and transfer payloads are not stored there.
On Unix, the config directory is set to `0700` and the config file is set to `0600`.

Service files are installed to:

```text
macOS: ~/Library/Application Support/org.lan-cat.lan-cat/org.lan-cat.daemon.plist
Linux: ~/.config/systemd/user/lan-cat.service
```

Integration files are installed to:

```text
macOS: ~/Library/Services/Share with lan-cat.workflow
Linux: ~/.config/Thunar/uca.xml
```

## Explicit file sharing

`lan-cat share` opens a native GUI without changing the clipboard. Select a connected peer, review
the files, and start the transfer. The receiving device shows an accept/reject dialog with an
editable destination folder. Both sides show byte progress, transfer speed, errors, and cancellation.

Files are sent as 48 KiB encrypted chunks. Every chunk requires an authenticated acknowledgement
before the next chunk is sent. The receiver writes hidden `.part` files and atomically renames each
completed file. Existing destination files are never overwritten. Transfers support up to 256 files
and 100 GiB total; resume after daemon restart is not yet supported.

Copying files normally in Finder or Thunar opens a two-button confirmation window on the device
where the copy happened. **Normal copy only** is selected by default and sends nothing. Use Up/Down
or Tab to select, Enter to confirm, and Esc to close with the default normal-copy behavior. Mouse
clicks also work. **Sync clipboard** sends the file clipboard payload through normal clipboard
synchronization so it can be pasted on another device. This path retains the clipboard limit of 64
files and 16 MiB total. The receiver should not open another confirmation popup for remote-injected
file clipboard data. The separate **Share with lan-cat** action uses the large-file transfer
protocol.

The confirmation uses the Wayland app ID `lan-cat-copy-prompt`, has no window decorations, and
requests always-on-top. Wayland compositors control whether a window floats, so add the matching
rule for your compositor:

```text
# Sway
for_window [app_id="lan-cat-copy-prompt"] floating enable, move position center

# Hyprland 0.55+ (Lua config)
hl.window_rule({
  match = { class = "lan-cat-copy-prompt" },
  float = true,
  center = true,
})
```

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
- Copied directories are rejected by clipboard sync. Use explicit file sharing after selecting
  regular files.
- Cursor uses a dedicated encrypted UDP service; clipboard and file sync remain on protocol v4 TCP.
- Stop daemon before enabling cursor discovery. Paired online peers are detected automatically;
  keep pressing any edge for three seconds to confirm entry on peer's opposite edge. Reversing
  direction cancels fluid edge preview.
- macOS requires Accessibility and Input Monitoring permissions. Linux Wayland requires layer-shell,
  relative-pointer, pointer-constraints, and wlroots virtual-pointer protocols.
- Cursor sharing controls pointer input and in-app drag operations. Dragging files between Finder
  instances is not yet supported; use `lan-cat share` for files.
- Clipboard protocol remains v4. Cursor UDP protocol is independently versioned.
- Existing clipboard content is captured at startup and replayed to peers during the same daemon run.
- Latest in-memory event is sent when a peer reconnects during the same daemon run.
- Pause discards events; resume takes a new baseline and does not replay old content.
- Concurrent copies converge using version vectors and device-ID tie-breaking.
- Pair discovery expires after two minutes. Normal discovery remains LAN-local via mDNS.

Enable automatic cursor discovery on both devices:

```sh
lan-cat service stop
lan-cat cursor enable
lan-cat service start
```

Allow multicast DNS (UDP 5353), local TCP traffic, and UDP 4242 cursor traffic in host firewalls.
Normal sync ports are dynamically selected.
