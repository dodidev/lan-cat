# TODO

This document tracks planned features and enhancements for `lan-cat`. The current implementation is functional and stable for macOS and Linux Wayland platforms.

## Current Status

### ✅ Implemented (v0.1.0)
- **Core functionality**:
  - Clipboard sync: text, HTML, RTF, PNG, files
  - Version vector clocks for causal consistency
  - File transfer protocol with progress tracking
  - Pause/resume clipboard synchronization
- **Security**:
  - Noise XX pairing with 6-digit code confirmation
  - Noise KK authenticated encryption
  - X25519 key pairs with BLAKE3-based device IDs
  - Key pinning and peer trust database
- **Platforms**:
  - macOS 13+ via NSPasteboard
  - Linux Wayland via ext-data-control-v1 and wlr-data-control-v1
- **Features**:
  - mDNS-based LAN discovery
  - Opt-in input sharing (cursor + keyboard) on screen edges
  - Full keyboard support with modifier keys (Shift, Ctrl, Alt, Cmd/Super)
  - Mouse operations: movement, clicks, scrolling, drag operations
  - Edge-based input transfer with visual beacon feedback
  - Drag-drop file transfer between devices (POC, needs refinement)
  - GUI for file sharing and transfer management
  - File manager integration (Finder, Thunar)
  - User service support (launchctl, systemd)
  - IPC daemon with CLI commands

## High Priority

### Drag-Drop File Transfer (POC → Production)
**Status**: Proof-of-concept implemented in v0.1.0, needs refinement for production use.

**Goal**: Complete and stabilize drag-drop file transfer between devices during input sharing sessions.

**Current POC limitations**:
- [ ] Improve drag operation detection reliability
  - Better differentiation between local drags and cross-device drags
  - Handle edge cases where drag is cancelled mid-operation
- [ ] Polish file transfer initiation
  - Seamless transition from drag to transfer protocol
  - Clear visual feedback during transfer initiation
  - Handle large file sets more efficiently
- [ ] Enhanced drop zone visualization
  - Better drop target highlighting on receiving device
  - Show file preview/count during drag operation
  - Provide clear accept/reject UI for recipient
- [ ] Cross-platform consistency:
  - Ensure consistent behavior on macOS and Wayland
  - Handle platform-specific drag payload formats
  - Test with various file types and sizes
- [ ] Error handling and recovery:
  - Graceful fallback if drag-drop fails
  - Clear error messages to user
  - Proper cleanup on cancellation
- [ ] Performance optimization:
  - Reduce latency for drag operation detection
  - Maintain UI responsiveness during large transfers
  - Optimize network usage for drag feedback

**Rationale**: POC demonstrates feasibility, but production quality requires robust error handling, cross-platform consistency, and polished UX.

### Pairing and Trust Verification
**Goal**: Improve security UX by making device fingerprints visible and verifiable.

- [ ] Show device fingerprint in `lan-cat status` output
  - Display as hex or base64-encoded BLAKE3 hash of public key
  - Include QR code option for easier comparison
- [ ] Show peer fingerprints in `lan-cat peers` output
- [ ] Add `lan-cat join <host-or-domain>` for direct pairing without mDNS
  - Support direct IP addresses and `.local` domain names
  - Useful for mDNS-restricted networks or manual configuration
  - Keep mDNS discovery as default and recommended path
- [ ] Document trust model clearly in README:
  - Fingerprints MUST be compared out-of-band before pairing
  - Six-digit code alone is vulnerable to MitM in adversarial LANs
  - Recommended: voice call, secure messaging, or in-person comparison

**Rationale**: Current pairing relies on short authentication codes which may be insufficient for adversarial network environments. Device fingerprints provide long-term identity verification.

### Local Hostname Configuration
**Goal**: Allow users to customize mDNS advertise name independently of system hostname.

- [ ] Add `advertise_hostname` field to config.json schema
- [ ] Separate display name (for UI) from advertise hostname (for mDNS)
- [ ] Validate advertise names before saving:
  - Allow: letters (a-z), digits (0-9), hyphen (-), dot (.)
  - Reject: empty labels, names > 253 chars, labels > 63 chars
  - Enforce DNS label restrictions (no leading/trailing hyphens)
- [ ] Use configured hostname in mDNS service advertisement when set
- [ ] Add CLI commands:
  - `lan-cat hostname` - display current advertise hostname
  - `lan-cat hostname <value>` - set advertise hostname
  - `lan-cat hostname --reset` - revert to system hostname
- [ ] Keep `lan-cat name` for display name (shown in peer list)

**Rationale**: Users may want different advertise names than their system hostname, especially for privacy or multi-device scenarios.

### Enhanced Configuration Management
**Goal**: Make config.json schema explicit and add debugging tools.

- [ ] Version config schema (currently implicit v1)
- [ ] Add migration framework for future schema changes
- [ ] Explicit schema for new optional fields:
  - `advertise_hostname` (string, optional)
  - `discovery_enabled` (bool, default true)
  - `clipboard_backend` (string, auto-detect or manual override)
  - `integration_installed` (bool, track installation state)
- [ ] Add CLI commands:
  - `lan-cat config path` - print config file location
  - `lan-cat config show` - display current config (redact private key)
  - `lan-cat config validate` - check config integrity
- [ ] Never store sensitive data:
  - No clipboard content in config or logs
  - No transfer payloads persisted
  - Only identity private key allowed (required for operation)

**Rationale**: Explicit schema enables better error messages, easier debugging, and smoother upgrades.

## Medium Priority

### X11 Clipboard Backend
**Goal**: Support Linux desktops using X11 display server.

**Requirements**:
- [ ] Detect `DISPLAY` environment variable
- [ ] Implement clipboard backend using `x11-clipboard` or similar crate
- [ ] Support clipboard formats:
  - `text/plain` (UTF-8)
  - `text/html`
  - `image/png`
  - `text/uri-list` for copied files
- [ ] Non-blocking operation (don't block daemon on clipboard owner changes)
- [ ] Duplicate suppression for remote writes (prevent sync loops)
- [ ] Test with common X11 file managers (Thunar, PCManFM, Nautilus)

**Challenges**:
- X11 clipboard ownership model differs from Wayland/macOS
- Must handle clipboard owner changes gracefully
- Selection vs primary clipboard considerations

**Rationale**: Many Linux users still run X11, particularly on older or enterprise systems.

### Windows Support
**Goal**: Extend lan-cat to Windows desktops for cross-platform compatibility.

**Requirements**:
- [ ] Windows clipboard backend:
  - Win32 Clipboard API integration
  - Support: CF_TEXT, CF_UNICODETEXT, CF_HTML, CF_DIBV5, CF_HDROP
  - PNG conversion from DIB format
  - File drop list (HDROP) to file paths
- [ ] Duplicate suppression for remote clipboard writes
- [ ] Service installation:
  - Windows Task Scheduler or service registration
  - Auto-start on login option
- [ ] File Explorer integration:
  - Context menu for "Share with lan-cat"
  - Registry-based installation
- [ ] Test cross-platform:
  - Windows ↔ macOS
  - Windows ↔ Linux Wayland
  - Windows ↔ Linux X11

**Challenges**:
- Windows clipboard format conversion (DIB vs PNG)
- Service installation without admin privileges
- File Explorer shell extension complexity

**Rationale**: Windows remains dominant in enterprise and gaming environments.

### IPC and Daemon Lifecycle Improvements
**Goal**: Improve reliability and error reporting for daemon operations.

- [ ] Stale socket cleanup on daemon start
  - Detect if previous daemon crashed without cleanup
  - Remove orphaned socket files automatically
- [ ] Better error messages when daemon is not running:
  - Current: generic "connection refused"
  - Desired: "lan-cat daemon is not running. Start with: lan-cat service start"
- [ ] Add `lan-cat daemon --foreground` flag:
  - Run without daemonizing (useful for debugging)
  - Log to stdout instead of system logger
  - Exit on terminal close (don't persist)
- [ ] IPC socket permission validation:
  - Verify socket is owned by current user
  - Reject connection if socket permissions are too permissive
- [ ] Daemon PID file:
  - Write PID to known location
  - Clean up stale PID files on start

**Rationale**: Improved robustness for service crashes and clearer error messages for users.

### Extended File Manager Integration
**Goal**: Support more file managers beyond Finder and Thunar.

**Linux**:
- [ ] Dolphin (KDE) custom actions
- [ ] Nemo (Cinnamon) actions
- [ ] PCManFM (LXDE) actions
- [ ] Test: ensure "Share with lan-cat" appears in all supported file managers

**macOS**:
- [ ] Consider Finder Sync extension (requires signing and sandboxing)
- [ ] Evaluate if Quick Actions are sufficient (likely yes for v1.0)

**Windows** (future):
- [ ] Explorer context menu via registry
- [ ] COM-based shell extension (more robust but complex)

**Rationale**: Broader file manager support improves user experience, but Finder/Thunar cover majority use cases currently.

## Low Priority / Future

### Additional Clipboard Formats
- [ ] SVG clipboard support (image/svg+xml)
- [ ] TIFF image support (image/tiff)
- [ ] PDF clipboard support (application/pdf)
- [ ] Rich text with embedded images
- [ ] Copy directory as archive (tar.gz or zip)

**Note**: These formats can already be shared by copying as regular files, so priority is lower.

### Advanced Configuration
- [ ] Per-peer clipboard sync enable/disable
- [ ] Per-peer cursor sharing enable/disable
- [ ] Clipboard format filters (e.g., only sync text, exclude images)
- [ ] Transfer history retention policy (auto-delete old transfers)
- [ ] Bandwidth throttling for file transfers

### Protocol Enhancements
- [ ] Protocol versioning and negotiation
- [ ] Backward compatibility with older protocol versions
- [ ] Compressed clipboard payloads (zstd) for large text
- [ ] Chunked clipboard transfers for very large payloads
- [ ] Resume interrupted file transfers

### Discovery Improvements
- [ ] Manual peer addition via IP address (without mDNS)
- [ ] Remember last-known IP addresses for faster reconnection
- [ ] IPv6 support validation
- [ ] Firewall configuration detection and guidance

## Testing and Quality Assurance

### Automated Tests
- [ ] Protocol message serialization/deserialization tests
- [ ] Version vector clock correctness tests
- [ ] Clipboard payload validation tests
- [ ] File transfer protocol state machine tests
- [ ] Mock backend tests for clipboard managers

### Manual Test Matrix
- [ ] macOS → Linux Wayland (all clipboard formats)
- [ ] Linux Wayland → macOS (all clipboard formats)
- [ ] macOS → Linux X11 (when X11 backend implemented)
- [ ] Linux X11 → macOS (when X11 backend implemented)
- [ ] Windows → macOS (when Windows support implemented)
- [ ] Windows → Linux (when Windows support implemented)
- [ ] Multi-device sync (3+ devices)
- [ ] Network interruption recovery
- [ ] Daemon restart persistence

### Edge Cases
- [ ] Very large clipboard payloads (near 16 MB limit)
- [ ] Maximum file count (64 files)
- [ ] Files with unusual names (Unicode, special chars)
- [ ] Clipboard changes during active transfer
- [ ] Multiple simultaneous file transfers
- [ ] Cursor sharing edge detection accuracy
- [ ] mDNS service name collisions

### Performance
- [ ] Benchmark clipboard sync latency
- [ ] Benchmark file transfer throughput
- [ ] Memory usage profiling under load
- [ ] CPU usage during idle and active sync

## Documentation

### User Documentation
- [x] Comprehensive README with all commands
- [x] Architecture overview
- [x] Security model explanation
- [ ] Troubleshooting guide
- [ ] FAQ for common issues
- [ ] Platform-specific setup guides

### Developer Documentation
- [ ] Protocol specification document
- [ ] Message format reference
- [ ] Backend implementation guide
- [ ] Contributing guidelines
- [ ] Release process documentation

---

## Notes

**Version Targeting**:
- v0.1.x: Current implementation (macOS + Wayland)
- v0.2.x: Enhanced pairing, configuration, X11 support
- v0.3.x: Windows support
- v1.0.x: Stable release with comprehensive testing

**Protocol Stability**:
- Current protocol v4 is not guaranteed stable
- Breaking changes allowed before v1.0
- Pairing reset may be required on protocol upgrades

**Philosophy**:
- Privacy and security first
- Simple, predictable behavior
- Platform-native integration
- Zero-configuration default experience
- Power-user customization available
