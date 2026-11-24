# Browsers and Profile Cloning

`agent-box` works with native Linux installs of six Chromium-family browsers. It auto-detects the browser you used most recently, clones its profile with copy-on-write, and re-encrypts cookies in memory so the boxed browser opens signed in without touching your desktop keyring.

## Supported browsers

| Kind | Native binary candidates | Profile directory (`$XDG_CONFIG_HOME` / `~/.config`) | Keyring `application=` lookup |
| --- | --- | --- | --- |
| `helium` | `/opt/helium-browser-bin/helium`, `helium`, `helium-browser` | `net.imput.helium` | `helium`, `chromium` |
| `chromium` | `/usr/bin/chromium`, `/usr/bin/chromium-browser`, `chromium`, `chromium-browser` | `chromium` | `chromium` |
| `chrome` | `/opt/google/chrome/chrome`, `google-chrome-stable`, `google-chrome` | `google-chrome` | `chrome`, `chromium` |
| `brave` | `/opt/brave.com/brave/brave`, `brave-browser`, `brave` | `BraveSoftware/Brave-Browser` | `brave`, `chromium` |
| `vivaldi` | `/opt/vivaldi/vivaldi`, `vivaldi`, `vivaldi-stable` | `vivaldi` | `vivaldi`, `chromium` |
| `edge` | `/opt/microsoft/msedge/msedge`, `microsoft-edge`, `microsoft-edge-stable` | `microsoft-edge` | `microsoft-edge`, `chromium` |

### How auto-detection picks a browser

1. `agent-box` checks each row above for a native executable binary.
2. Among installed native browsers, it selects the one whose `<data_dir>/Local State` file has the newest modification timestamp.
3. If none of the installed browsers has a `Local State` file yet, it picks the first installed native browser in table order.
4. Profile lookups honor `XDG_CONFIG_HOME` when set and non-empty, falling back to `~/.config`.

## Selecting a browser or profile manually

Use flags on `agent-box up` to override auto-detection:

```bash
# Clone a specific browser data directory and sub-profile
agent-box up --from ~/.config/google-chrome --profile "Profile 1"

# Pin a specific native browser binary (name on PATH or absolute path)
agent-box up --browser brave-browser --url https://github.com

# Start a fresh browser with no cloned cookies or state
agent-box up --blank --url https://example.com

# Start a desktop with no browser (launch other apps with `agent-box run`)
agent-box up --no-browser

# Include browser extensions and extension storage (skipped by default)
agent-box up --with-extensions
```

Behavior details:

- `--profile` defaults to `profile.last_used` from `Local State`, falling back to `Default`. It must be a single directory name inside the browser data directory.
- Passing `--from` with a recognized directory name (such as `google-chrome` or `BraveSoftware/Brave-Browser`) automatically pairs it with the matching native binary and keyring application names.
- Passing `--from` with a custom directory path falls back to the auto-detected native browser and checks all six keyring application names (`helium`, `chrome`, `brave`, `vivaldi`, `microsoft-edge`, `chromium`).
- Passing `--browser` with a recognized binary name infers its default data directory unless `--from` or `--blank` is given.

## Snap and Flatpak confinement

Snap-packaged and Flatpak-packaged browsers are rejected at detection and validation time. `agent-box` inspects the binary path, symlink chains, canonical executable name, and shell wrappers invoking `snap run` or `flatpak run`.

Confined packages break box isolation because their launchers re-attach to host portals, host D-Bus paths, or confined cgroups instead of staying inside the box's private X11 display, private session bus, and systemd slice.

Install a native system package instead:

- **Ubuntu / Debian**: Install Google Chrome from its `.deb`, Brave or Vivaldi from their native apt repositories, or native Chromium from Debian (`apt install chromium`). Ubuntu's `chromium-browser` package is a Snap wrapper.
- **Fedora**: Install `chromium` (`dnf install chromium`) or vendor `.rpm` packages for Chrome, Brave, Vivaldi, or Edge.
- **Arch Linux**: Install `chromium` (`pacman -S chromium`) or native packages for Helium, Brave, Chrome, or Vivaldi.

## Live-browser SQLite consistency

Your main browser can remain open while `agent-box up` clones its profile. Because Chromium writes to `Cookies` (or `Network/Cookies`) while running, `agent-box` copies both the database file and its rollback journal (`Cookies-journal`) using copy-on-write reflinks (`FICLONE`), then opens the copy and runs `PRAGMA quick_check`.

If the live browser was mid-commit and the copy fails `quick_check`, `agent-box` re-copies the database and journal up to 5 times with a 200 ms pause between attempts. It also rewrites `Preferences` (`exit_type: "Normal"`, `exited_cleanly: true`) and trims `Local State` so the boxed browser starts cleanly without a crash-restore prompt or profile picker.

## Cookie database versions and encryption (`v10`, `v11`, `v12`)

- **Schema version**: `agent-box` supports Chromium Cookie database versions `1` through `24`. On version `24`, Chromium prefixes each decrypted cookie value with `SHA-256(host_key)`; `agent-box` verifies this 32-byte prefix before accepting a candidate keyring key. Newer schemas (`> 24`) fail loudly rather than corrupting cookies.
- **`v11` to `v10` re-encryption**: On Linux with Secret Service (GNOME Keyring), Chromium encrypts cookies with a keyring-backed AES-128-CBC key (`v11`). `agent-box` fetches the unlocked secret via `secret-tool search`, decrypts `v11` rows in memory, and re-encrypts them under Chromium's fixed basic key (`v10`, derived from `"peanuts"`). The boxed browser then starts with `--password-store=basic` and needs no keyring access.
- **`v12` cookies**: Cookies encrypted with other prefixes (such as `v12` from `xdg-desktop-portal` Secret portal) are rejected with an error instead of silently vanishing inside the box. See [troubleshooting.md](troubleshooting.md) for workarounds.

## KDE KWallet status

In `v0.1`, cookie decryption uses `secret-tool search` (FreeDesktop Secret Service / GNOME Keyring). Native KDE KWallet backends that do not expose unlocked items over Secret Service are not yet supported. When `agent-box` detects a KDE session (`XDG_CURRENT_DESKTOP` containing `kde` or non-empty `KDE_FULL_SESSION`) and cannot decrypt `v11` cookies, it reports that KWallet is unsupported in `v0.1` and suggests GNOME Keyring / Secret Service or `--blank`.

## Driving boxed browsers with CDP

Every boxed browser starts with `--remote-debugging-address=127.0.0.1 --remote-debugging-port=0` and writes its ephemeral port to `DevToolsActivePort`. Retrieve the endpoint at any time:

```bash
CDP=$(agent-box cdp box1)
# http://127.0.0.1:41923
```

### With `agent-browser`

```bash
CDP=$(agent-box cdp box1)
agent-browser --cdp "$CDP" snapshot -i
agent-browser --cdp "$CDP" click @e3
agent-browser --cdp "$CDP" screenshot /tmp/page.png
```

### With Playwright

```js
import { chromium } from "playwright";

const browser = await chromium.connectOverCDP(process.env.CDP_URL);
const context = browser.contexts()[0];
const page = context.pages()[0] ?? (await context.newPage());
await page.goto("https://github.com");
console.log(await page.title());
```
