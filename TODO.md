# TODO

## Pairing and identity

- Add join flow with fingerprint confirmation.
- Show device fingerprint in `lan-cat status` and `lan-cat peers`.
- Support `lan-cat join <host-or-domain>` for direct pairing without mDNS discovery.
- Keep current mDNS pairing as default LAN discovery path.
- Document trust model: fingerprint must be compared out of band before accepting a peer.

## Local hostname/domain

- Add local host/domain configuration, for example `skynet.local`.
- Store configured advertise name in config instead of deriving only from hostname.
- Validate local names before saving:
  - allow letters, digits, hyphen, dot;
  - reject empty labels;
  - reject names longer than DNS limits.
- Use configured name in mDNS service instance/host advertisement when available.
- Add CLI commands:
  - `lan-cat name <value>` for display name;
  - `lan-cat host <value>` for local advertise hostname/domain;
  - `lan-cat host` to print current host setting.

## Local configuration

- Make config schema explicit for:
  - display name;
  - advertise hostname/domain;
  - discovery enable/disable;
  - clipboard backend preference;
  - file-manager integration preference.
- Add migration path for existing `config.json`.
- Add `lan-cat config path` and `lan-cat config show` for debugging.
- Avoid storing clipboard content, transfer payloads, or secrets beyond existing private key.

## Clipboard backends

- Keep current macOS `NSPasteboard` backend.
- Keep current Linux Wayland data-control backend.
- Add Linux X11 backend:
  - detect `DISPLAY`;
  - support text, HTML, PNG, and copied file URI lists;
  - avoid blocking daemon on clipboard owner changes;
  - test with common X11 file managers.
- Add Windows backend:
  - use Win32 Clipboard API;
  - support text, HTML, PNG/DIB conversion, and file drop lists;
  - preserve remote-injected clipboard suppression so sync does not loop;
  - add Windows service/startup story.

## Platform integration

- Linux:
  - keep Thunar integration;
  - consider Dolphin/Nemo/PCManFM actions after core protocol is stable.
- macOS:
  - keep Finder Quick Action;
  - consider Finder extension only if signing/bundle work becomes acceptable.
- Windows:
  - add Explorer context-menu integration for explicit file share.

## Verification

- Add backend-specific tests where platform APIs allow it.
- Add manual test matrix:
  - macOS to Wayland;
  - Wayland to macOS;
  - macOS to X11;
  - X11 to macOS;
  - Windows to macOS;
  - Windows to Linux;
  - Linux to Windows.
- Verify copy loops are suppressed for remote file clipboard writes on every backend.
