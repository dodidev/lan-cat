# lan-cat

Secure, peer-to-peer clipboard sync, input sharing, and LAN file transfer for macOS and Linux Wayland desktops.

## Overview

`lan-cat` provides seamless clipboard synchronization, remote input control (cursor and keyboard), and file sharing between trusted devices on your local network. It uses modern cryptography (Noise protocol) to ensure security and privacy while maintaining a simple user experience.

**Key principles:**
- **Privacy-first**: No cloud, no accounts, no telemetry. All data stays on your LAN.
- **Trust-based**: Explicit pairing with code confirmation. Only trusted peers can sync.
- **Platform-native**: Direct integration with macOS NSPasteboard and Linux Wayland data-control.
- **User-controlled**: Explicit confirmation for file transfers and cursor sharing.

## Features

### Clipboard Synchronization
- **Supported formats**: Text, HTML, RTF, PNG images, and copied regular files
- **Real-time sync**: Automatic synchronization across all paired devices
- **Conflict resolution**: Version vector clocks prevent clipboard loops and ensure causal consistency
- **Size limits**: Up to 16 MB payloads, maximum 64 files per clipboard operation
- **File handling**: Temporary materialized files for paste operations, automatic cleanup

### Explicit File Sharing
- **GUI-based**: Select files and choose destination peer through native GUI
- **Progress tracking**: Real-time transfer progress, speed, and estimated time
- **Safe writes**: Files written to `.part` extensions during transfer, renamed on completion
- **User consent**: Accept/reject transfers on receiving end with destination selection
- **Cancellation**: Either side can cancel in-progress transfers
- **Integration**: Finder Quick Action (macOS) and Thunar custom action (Linux)
- **Drag-drop transfer**: POC support for dragging files between devices during input sharing (experimental)

### Input Sharing (Opt-in)
- **Cursor and keyboard**: Full mouse and keyboard control of remote device
- **Edge-based**: Push cursor to screen edges to transfer input control
- **Complete input**: Mouse movement, clicks, scrolling, drag operations, and keyboard typing
- **Modifier keys**: Support for Shift, Control, Alt/Option, Command/Super keys
- **Bidirectional**: Works on all four screen edges with visual feedback
- **Beacon UI**: Shows edge detection progress and peer availability
- **Platform support**: macOS (requires Accessibility permissions) and Wayland compositors
- **Security**: Opt-in only due to remote input injection capability

### Service Management
- **User services**: Manual-start services via `launchctl` (macOS) or `systemd --user` (Linux)
- **Automatic restart**: Service restart on failure with 3-second delay
- **Daemon mode**: Background process with IPC for CLI commands
- **Status monitoring**: Real-time status checks and peer listing

## Security Model

### Cryptography
- **Pairing**: Noise XX handshake with 6-digit authentication code confirmed on both devices
- **Connections**: Noise KK authenticated encryption for all subsequent connections
- **Identity**: X25519 key pairs, device IDs derived from public key BLAKE3 hash
- **Key pinning**: Peer public keys stored in config; unknown devices rejected automatically

### Privacy Guarantees
- **No logging**: Clipboard content never written to logs or config files
- **Temporary files**: Received clipboard files use private temporary directories (mode 0700)
- **Cleanup**: Temporary files removed on expiration or daemon exit
- **Safe transfers**: Explicit file transfers use hidden `.part` files until completion
- **No network services**: No cloud, accounts, relays, telemetry, or internet connectivity

### Threat Model
- **Protects against**: LAN eavesdropping, packet tampering, device impersonation
- **Does NOT protect against**: Malware or software running as your local user
- **Trust boundary**: Clipboard data shared with explicitly paired devices only
- **Configuration security**: Config stored at `~/.config/lan-cat/config.json` (mode 0600)

### Protocol Details
- **Version**: Protocol v4 (backward compatibility not guaranteed during development)
- **Transport**: TCP with ChaCha20-Poly1305 AEAD encryption
- **Discovery**: mDNS for LAN-only device discovery
- **Message format**: CBOR serialization with length-prefixed framing

## Platform Support

### macOS
- **Requirements**: macOS 13 (Ventura) or newer
- **Clipboard backend**: `NSPasteboard` API via `objc2`
- **Service manager**: `launchctl` user services
- **File manager**: Finder Quick Actions (`.workflow` bundles)
- **Input injection**: CoreGraphics events (cursor + keyboard), requires Accessibility permissions

### Linux (Wayland)
- **Requirements**: Wayland compositor with data-control protocol support
- **Clipboard backend**: `wl-clipboard-rs` with protocol preference:
  1. `ext-data-control-v1` (modern standard)
  2. `wlr-data-control-v1` (wlroots fallback)
- **Supported compositors**: KDE Plasma, Sway, Hyprland, niri, River, and similar
- **Unsupported**: GNOME/Mutter (no background data-control protocol)
- **Service manager**: `systemd --user` services
- **File manager**: Thunar custom actions (XML-based)
- **Input injection**: Virtual pointer and virtual keyboard protocols (zwlr/zwp)

### Limitations
- **No X11**: X11 clipboard backend not yet implemented (see TODO.md)
- **No Windows**: Windows support planned but not implemented
- **No directories**: Copied directories not supported (copy as archive file instead)
- **Format restrictions**: SVG, TIFF, and PDF clipboard formats not supported
  - Workaround: Copy these as regular files instead

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

### Additional Commands

**Status and peer management:**
```sh
lan-cat status                      # Show daemon status, platform info, and backend
lan-cat peers                        # List all paired devices with IDs and names
lan-cat name                         # Display current device name
lan-cat name "My Laptop"             # Change device name
lan-cat unpair <peer-id-prefix>      # Remove a paired peer
```

**Clipboard control:**
```sh
lan-cat pause                        # Pause synchronization (daemon keeps running)
lan-cat resume                       # Resume synchronization from fresh baseline
```

**File sharing:**
```sh
lan-cat share file1.txt file2.pdf    # Open GUI to select peer and share files
lan-cat share --peer <id> file.zip   # Share directly with specific peer
lan-cat transfers                    # Open transfer history window
```

**Input sharing (cursor and keyboard):**
```sh
lan-cat cursor enable                # Enable input sharing on screen edges
lan-cat cursor disable               # Disable input sharing
lan-cat cursor status                # Show current input sharing configuration
```

**File manager integration:**
```sh
lan-cat integration install          # Install Finder/Thunar sharing action
lan-cat integration uninstall        # Remove file manager integration
```

## Architecture

### Components
```
┌─────────────────────────────────────────────────────────┐
│                   CLI Commands                          │
└───────────────────┬─────────────────────────────────────┘
                    │ IPC (Unix socket)
┌───────────────────▼─────────────────────────────────────┐
│                    Daemon                               │
│  ┌──────────────┬──────────────┬──────────────┐        │
│  │  Clipboard   │   Network    │  Transfer    │        │
│  │   Manager    │   Manager    │   Manager    │        │
│  └──────┬───────┴──────┬───────┴──────┬───────┘        │
│         │              │              │                 │
│  ┌──────▼──────┐ ┌────▼────┐  ┌──────▼──────┐         │
│  │  Backend    │ │  mDNS   │  │   Transfer  │         │
│  │  (macOS/    │ │  Noise  │  │   Protocol  │         │
│  │  Wayland)   │ │  TCP    │  │   (CBOR)    │         │
│  └─────────────┘ └─────────┘  └─────────────┘         │
└─────────────────────────────────────────────────────────┘
         │                │                │
┌────────▼────────┐ ┌─────▼──────┐ ┌──────▼────────┐
│  NSPasteboard/  │ │  Network   │ │  Filesystem   │
│  data-control   │ │    (LAN)   │ │  (transfers)  │
└─────────────────┘ └────────────┘ └───────────────┘
```

### Module Overview
- **`main.rs`**: CLI argument parsing and command dispatch
- **`daemon.rs`**: Main event loop, coordinates clipboard/network/transfer managers
- **`config.rs`**: Configuration persistence, key management
- **`network.rs`**: mDNS discovery, Noise handshakes, secure connections
- **`protocol.rs`**: Message types, clipboard payload validation
- **`clipboard/`**: Platform-specific clipboard backends (macOS, Wayland)
- **`transfer/`**: File transfer protocol, progress tracking
- **`input/`**: Input sharing implementation - cursor and keyboard (platform-specific)
- **`ipc.rs`**: Unix socket IPC between CLI and daemon
- **`ordering.rs`**: Version vector clocks for causal consistency
- **`gui.rs`**: Native GUI windows for file sharing and transfers
- **`integration.rs`**: File manager integration installation
- **`service.rs`**: System service installation and management

### Data Flow
1. **Clipboard change** detected by platform backend
2. **Payload extracted** and validated against size/format limits
3. **Version vector** incremented for causal ordering
4. **ClipboardEvent** broadcast to all active peer connections
5. **Receiving peers** compare version vectors
6. **Remote payload** applied to local clipboard if causally newer
7. **Duplicate suppression** prevents sync loops

### Configuration
- **Location**: `~/.config/lan-cat/config.json` (Linux/macOS)
- **Permissions**: Mode 0600 (private to user)
- **Contents**:
  - Version and device name
  - X25519 private/public key pair
  - Trusted peer database (ID → name, public key)
  - Pause state and cursor settings
  - Version vector clock state

## Development

### Build Requirements
- **Rust**: Edition 2024, MSRV 1.85
- **macOS**: Xcode command line tools
- **Linux**: Wayland development libraries

### Dependencies
- **Crypto**: `snow`, `blake3`, `chacha20poly1305`, `x25519-dalek`
- **Async**: `tokio` (full features)
- **Network**: `mdns-sd` for discovery
- **GUI**: `eframe` with Wayland/X11/macOS backends
- **Platform**: `objc2` (macOS), `wl-clipboard-rs` (Linux)

### Testing
```sh
cargo test
cargo build --release
```

### Debugging
```sh
# Enable trace logging
export RUST_LOG=lan_cat=trace
lan-cat daemon

# Check service status
lan-cat service status

# View config
cat ~/.config/lan-cat/config.json
```
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

`lan-cat name` prints the current device name. `lan-cat name <new-device-name>` stores a printable
1..63 character name used for discovery and peer lists.

Service files are installed to:

```text
macOS: ~/Library/Application Support/org.lan-cat.lan-cat/org.lan-cat.daemon.plist
Linux: ~/.config/systemd/user/lan-cat.service
```

The daemon IPC socket is local-user only:

```text
macOS: ~/Library/Caches/org.lan-cat.lan-cat/lan-cat.sock
Linux: $XDG_RUNTIME_DIR/lan-cat.sock, or ~/.cache/lan-cat/lan-cat.sock when unset
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
