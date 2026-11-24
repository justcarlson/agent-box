# Security Model

`agent-box` provides **workspace isolation**, not a security sandbox. Processes inside a box run under your Linux user account (same UID, filesystem permissions, and network stack as your shell). Do not use `agent-box` to contain untrusted or hostile code.

For vulnerability disclosure, see [SECURITY.md](../SECURITY.md).

## Threat model: isolated vs. shared

| Surface | Status | Details |
| --- | --- | --- |
| Display and input | Isolated | Private `Xvfb` on `:100+` with random `MIT-MAGIC-COOKIE-1` (`0600`). Boxed apps cannot draw on, read, or steal focus from your host desktop (`:0` / Wayland). |
| Session D-Bus and AT-SPI | Isolated | Private `dbus-daemon` with no service activation directories, so portals, GVFS, and host daemons never auto-start inside a box. |
| Desktop keyring | Isolated | Keyring secret is read once in `agent-box up` and zeroed in memory. The box has no access to your host session bus or keyring. |
| Host browser profile writes | Isolated | Copy-on-write clone (`FICLONE`, fallback regular copy) with symlinks skipped so boxed writes never mutate `~/.config/<browser>`. |
| Cloud browser sync | Isolated | Boxed browsers launch with `--disable-sync`, preventing edits from syncing to cloud accounts. |
| Saved passwords and extensions | Isolated by default | `Login Data`, `Web Data`, and browser extensions are skipped unless `--with-extensions` is passed. |
| Linux UID and host filesystem | Shared | Boxed processes run as your UID. `HOME` is pointed at the box directory, but no mount or user namespace restricts direct reads of host paths. |
| Network | Shared | Boxed processes share the host network namespace and routes. |
| Loopback CDP port | Shared with local users | Chromium listens on `127.0.0.1:<ephemeral>` with no authentication. Any local user on the machine can reach loopback ports. |
| Server-side web sessions | Shared | Cloned cookies contain copies of live session tokens. Signing out or revoking sessions inside a box invalidates them on the host browser too. |

## Cookie storage and disk lifetime (`v10`)

To start a signed-in browser without granting access to your desktop keyring, `agent-box` decrypts cloned `v11` cookies in memory and re-encrypts them to Chromium's fixed `v10` ("basic") store using `PBKDF2-SHA1("peanuts", "saltysalt", 1, 16)` and AES-128-CBC with a 16-space IV. Because the `v10` key is public, cloned cookies on disk are plaintext-equivalent to any process running under your UID.

`agent-box` limits the exposure window of those files:

- **Owner-only permissions**: `~/.cache/agent-box/` and every directory inside a cloned profile are created with mode `0700`.
- **Backup exclusion**: `~/.cache/agent-box/CACHEDIR.TAG` is written automatically using the Cache Directory Tagging Specification so tools such as Borg, restic, and `tar --exclude-caches` skip cloned cookie copies.
- **Automatic reaping**: Each box's systemd unit sets `RuntimeMaxSec` (default `12h`) and `ExecStopPost=agent-box __reap <name>`. Whether a box ends via `agent-box down`, TTL expiration, or a crash, systemd stops every app unit in the slice and deletes both `~/.cache/agent-box/<name>` and `$XDG_RUNTIME_DIR/agent-box/<name>`.
- **Interrupt cleanup**: If `agent-box up` receives `SIGINT`, `SIGTERM`, or `SIGHUP` mid-clone, it aborts and deletes the partially created box directory before exiting.
- **Crash recovery**: Run `agent-box gc` after an unclean reboot or hard power loss to reap any stopped box directories left behind. `agent-box up` also reaps stopped boxes automatically before creating a new one.
- **Copy-on-write snapshots (btrfs)**: Host btrfs snapshots that include `~/.cache` can retain historical `v10` cookie copies even after a box is deleted. Place `~/.cache/agent-box` on its own excluded btrfs subvolume, or point `XDG_CACHE_HOME` at a `tmpfs` mount when running signed-in boxes.

## What cloning skips

During profile cloning, `agent-box` skips symlinks (so a link cannot point back into the live profile and allow write-through edits) and omits caches, session state, history, autofill/passwords, and lock files:

- **Caches**: `Cache`, `Code Cache`, `GPUCache`, `DawnGraphiteCache`, `DawnWebGPUCache`, `GrShaderCache`, `ShaderCache`, `Service Worker/CacheStorage`, `blob_storage`
- **Open tabs and sessions**: `Sessions`, `Sessions_Encrypted`, `Current Session`, `Current Tabs`, `Last Session`, `Last Tabs`
- **History and predictors**: `History`, `Visited Links`, `Top Sites`, `Favicons`, `Shortcuts`, `Network Action Predictor` (and their `-journal` files)
- **Saved passwords and autofill**: `Login Data`, `Login Data For Account`, `Web Data` (and their `-journal` files)
- **Runtime locks**: `LOCK`, `DevToolsActivePort`

### Default extension skipping and `Local Storage` caveat

By default, `agent-box` skips extension code and extension state so password-manager extensions, wallets, and privileged plugins do not enter the box:

- `Extensions`, `Local Extension Settings`, `Sync Extension Settings`
- `Extension State`, `Extension Rules`, `Extension Scripts`
- `Extension Cookies`, `Extension Cookies-journal`, `Managed Extension Settings`
- `Storage/ext` and any path entry whose filename starts with `chrome-extension_` (such as per-extension stores under `IndexedDB`)

Pass `--with-extensions` on `agent-box up` if an agent workflow requires extensions. Note that Chromium's shared `Local Storage` LevelDB is cloned to preserve web-app logins and may still contain entries written by `chrome-extension://` origins.

## Non-prompting keyring reads

`agent-box` reads the browser's `v11` key with:

```bash
timeout 5 secret-tool search application <app>
```

Because `--unlock` is never passed, `secret-tool search` returns only items from an already-unlocked keyring and never pops a password dialog on your desktop. The raw stdout buffer is overwritten with zeros immediately after extracting the secret, and the secret never leaves the `agent-box up` process.

## Loopback CDP on multi-user hosts

Each boxed browser exposes Chrome DevTools Protocol on `127.0.0.1` using an OS-assigned ephemeral port with no authentication token. On a multi-user Linux host, any other local user can connect to that loopback port, execute JavaScript in open tabs, or read session cookies. Do not run signed-in boxes on shared or untrusted multi-user hosts.

## Shared server-side sessions

Cloned cookies are copies of the same server-side sessions your main browser holds. Clicking **Sign out**, revoking a device, or changing an account security setting inside a box can terminate that session on your host browser as well. Instruct agents to leave accounts signed in and simply run `agent-box down <name>` when finished.
