# Troubleshooting

Start with `agent-box doctor`. It checks every runtime dependency, detects your Linux distro family from `/etc/os-release`, and prints the exact install command for any missing package.

## `agent-box doctor` dependency matrix

| Check | Required | Purpose | Arch (`pacman -S`) | Debian / Ubuntu (`apt install`) | Fedora (`dnf install`) |
| --- | --- | --- | --- | --- | --- |
| `Xvfb` | Yes | Headless X11 display server (`:100+`) | `xorg-server-xvfb` | `xvfb` | `xorg-x11-server-Xvfb` |
| `openbox` | Yes | Lightweight window manager | `openbox` | `openbox` | `openbox` |
| `dbus-daemon` | Yes | Private per-box session bus | `dbus` | `dbus` | `dbus-daemon` |
| `dbus-send` | Yes | Enable AT-SPI on the private bus | `dbus` | `dbus` | `dbus-tools` |
| `systemd-run` | Yes | Transient user units and slices | `systemd` | `systemd` | `systemd` |
| `secret-tool` | Yes (signed-in) | Read unlocked `v11` keyring secret | `libsecret` | `libsecret-tools` | `libsecret` |
| `at-spi-bus-launcher`, `at-spi2-registryd` | Yes | Accessibility bus and registry | `at-spi2-core` | `at-spi2-core` | `at-spi2-core` |
| `cua-driver` | Optional | GUI accessibility tree (`agent-box call`) | Optional install | Optional install | Optional install |
| `browser` & `profile` | Yes (unless `--no-browser`) | Native Chromium-family browser and `Local State` | e.g. `chromium` | Native `.deb` or `chromium` | e.g. `chromium` |
| `systemd user manager` | Yes | Active `systemctl --user` session | Running user session | Running user session | Running user session |

One-line dependency installs:

```bash
# Arch Linux
sudo pacman -S xorg-server-xvfb openbox dbus libsecret at-spi2-core

# Debian / Ubuntu
sudo apt install xvfb openbox dbus libsecret-tools at-spi2-core

# Fedora
sudo dnf install xorg-x11-server-Xvfb openbox dbus-daemon dbus-tools libsecret at-spi2-core
```

## Locked keyring or KDE KWallet

**Symptom**: `could not decrypt the profile's cookies with any keyring entry (...); is the keyring unlocked?`

`agent-box` runs `secret-tool search` without `--unlock` so background agents never pop a password prompt on your desktop.

Fixes:

1. Unlock your GNOME Keyring / Secret Service collection on the host desktop (or verify `secret-tool search application chromium` returns an entry).
2. If you run KDE Plasma with KWallet, note that `v0.1` does not read KWallet directly. Either use a Secret Service provider (such as `gnome-keyring-daemon` or KWallet's FreeDesktop Secret Service compatibility if it exposes the browser's `application=` item) or start clean boxes with `agent-box up --blank`.

## Unsupported `v12` cookies or newer schema version

**Symptom**: `cookies use unsupported encryption [("v12", ...)]; only v10 and v11 are supported` or `cookie database version 25 is not supported (max 24)`.

- `v12` cookies are created when Chromium uses the `xdg-desktop-portal` Secret backend instead of GNOME Keyring (`v11`). `agent-box` refuses them loudly rather than launching a box with missing logins.
- Use `agent-box up --blank` when you do not need cloned logins, or run your host browser with GNOME Keyring / `--password-store=gnome-libsecret` so new cookies use `v11`.
- If Chromium bumped its SQLite cookie schema above `24`, update `agent-box` to a release that supports the new schema.

## cgroup v2 `MemoryHigh` warning

**Symptom**: `agent-box: warning: could not set --mem 3G on agent-box-box1.slice (...); continuing without MemoryHigh`

Your user systemd manager does not have the cgroup v2 `memory` controller delegated. The box still starts and applies `CPUWeight=50` and `IOWeight=50`, just without a soft memory cap.

To enable memory delegation on systemd Linux hosts:

```bash
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload
```

Log out and back in for the updated `user@.service` delegation to take effect. Also verify `--mem` uses valid systemd syntax (such as `3G`, `512M`, `50%`, or `infinity`; lowercase `3g` or `3gb` is rejected).

## AT-SPI modal dialog timeout (exit code `124`)

**Symptom**: `agent-box: <tool> gave no answer in 60 s; a modal dialog may block the accessibility read. Target the dialog window, or take a shot.` (exit status `124`).

Some desktop apps and browsers block AT-SPI queries on their parent window while a modal dialog (confirm box, print dialog, or file chooser) is open. `agent-box call` enforces `--timeout` (default `60` seconds) and kills the worker process group on expiration.

Recovery:

1. Capture the screen with `agent-box shot <name>` to see what modal is blocking the display.
2. Target the dialog window directly or dismiss it with `press_key` (`Escape` or `Return`) or CDP.
3. Shorten `--timeout` (for example `--timeout 15`) when probing unfamiliar apps.

## GTK file picker vs. CDP file upload

Boxed apps run with `GTK_USE_PORTAL=0`, so clicking a web page's file input opens a classic GTK file chooser inside the box. That modal dialog can stall AT-SPI snapshots and adds fragile UI steps.

For browser file uploads, skip clicking the picker and upload through CDP instead:

```bash
CDP=$(agent-box cdp box1)
agent-browser --cdp "$CDP" upload 'input[type="file"]' /path/to/file.pdf
```

Playwright's `locator.setInputFiles('/path/to/file.pdf')` works the same way without opening a GTK dialog.

## Inspecting logs

- **Live box logs**: `~/.cache/agent-box/<name>/logs/` holds stdout and stderr for `xvfb`, `dbus`, `atspi`, `registryd`, `openbox`, and `cua-driver`.
- **Crash or failed startup logs**: When `agent-box up` fails or a box unit exits with `SERVICE_RESULT != success`, `agent-box` copies the box's logs to `$XDG_RUNTIME_DIR/agent-box-logs/<name>-<timestamp>/` (tmpfs, cleared at logout) before deleting `~/.cache/agent-box/<name>`.
- **systemd journal**: Inspect the desktop unit or browser unit directly:

```bash
journalctl --user -u agent-box-box1 --no-pager -n 100
journalctl --user -u 'agent-box-box1-browser-*' --no-pager -n 100
```
