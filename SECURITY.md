# Security Policy

## Scope and Threat Model

`agent-box` provides **workspace isolation**, not a security sandbox.

- **Same UID and host access:** Processes inside a box run under your Linux user account with the same filesystem, network, and process permissions as your shell. A box is not a VM, container, or UID sandbox, and must not be used to contain untrusted or hostile code.
- **What a box isolates:** Each box runs a private Xvfb display, D-Bus session bus, AT-SPI bus, window manager, and copy-on-write browser profile inside its own systemd slice. Boxed apps do not draw on your desktop, read your Wayland session, or write back to your main browser profile.

## Loopback CDP Exposure

Each boxed browser exposes Chrome DevTools Protocol (CDP) on `127.0.0.1` using an ephemeral port with no authentication. On a shared multi-user machine, any other local user can connect to loopback ports and control the boxed browser or read its session cookies. Do not run signed-in boxes on untrusted multi-user hosts.

## Cookie Storage and Disk Lifetime (`v10`)

To start a boxed browser without handing it access to your desktop keyring, `agent-box` re-encrypts cloned `v11` cookies to Chromium's fixed `v10` ("basic") key inside `~/.cache/agent-box/<name>/`:

- `~/.cache/agent-box/` is created with mode `0700` (owner-only).
- `~/.cache/agent-box/CACHEDIR.TAG` is written automatically so standard backup tools (Borg, restic, tar `--exclude-caches`) exclude cloned cookie copies.
- Stopping a box (`agent-box down`), letting its TTL expire, or crashing triggers systemd `ExecStopPost` (`agent-box __reap`) to delete the box directory.
- Run `agent-box gc` after a hard power loss or crash to clean up any stopped boxes whose files were not reaped.
- On copy-on-write filesystems such as btrfs, consider placing `~/.cache/agent-box` on its own subvolume (or pointing `XDG_CACHE_HOME` at tmpfs) so host snapshots do not retain historical `v10` cookie copies.

## Shared Server-Side Sessions

Cloned cookies carry copies of your live session tokens. Signing out of a site or revoking sessions inside a box can also sign out your main browser on the host. Browser sync is disabled (`--disable-sync`) so boxed state is not pushed to cloud sync accounts.

## Browser Extensions

Browser extensions and their storage (`Extensions`, `Local Extension Settings`, `Sync Extension Settings`, etc.) are skipped by default during profile cloning so password managers and extension state are not copied into boxes unless you explicitly pass `--with-extensions`.

## Reporting a Vulnerability

Please report security issues privately through GitHub Security Advisories:

https://github.com/justcarlson/agent-box/security/advisories/new
