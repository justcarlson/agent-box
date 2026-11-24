---
name: agent-box
description: "Run isolated, disposable Linux desktops for AI agents, optionally cloned from the user's signed-in Chromium-family browser profile. Use for parallel browser automation, signed-in web workflows, or GUI app tasks without touching the host desktop."
---

# agent-box

Give an agent its own throwaway Linux desktop (`Xvfb` + private D-Bus/AT-SPI + `openbox` + optional `cua-driver`) inside a systemd user slice. By default, `agent-box up` copy-on-write clones the user's most recently used Chromium-family browser profile (Helium, Chromium, Chrome, Brave, Vivaldi, or Edge) and re-encrypts `v11` cookies to `v10` in memory so the box opens signed in without sharing the desktop keyring or touching the host screen.

## Preflight

Verify the host once before creating boxes:

```bash
agent-box doctor
```

If dependencies are missing, install the packages printed by `doctor`.

## Core workflow

1. Start a box (auto-names `box1`, `box2`, ... unless you pass a name):

```bash
agent-box up --url https://github.com
```

Capture the JSON `name` and `cdp` fields from stdout. For an empty profile (no cloned cookies), add `--blank`. For a non-browser desktop app, add `--no-browser`. To include browser extensions (skipped by default), add `--with-extensions`.

2. Drive web pages over CDP (preferred for browser tasks):

```bash
CDP=$(agent-box cdp box1)
agent-browser --cdp "$CDP" snapshot -i
agent-browser --cdp "$CDP" click @e3
```

Playwright (`chromium.connectOverCDP(cdp)`) or any CDP client works against the same `http://127.0.0.1:<port>` endpoint.

3. Launch native Linux GUI apps or open extra URLs inside the box:

```bash
agent-box run box1 gnome-calculator
agent-box open box1 https://example.com
```

4. Drive native apps via `cua-driver` (when `cua-driver` is installed on `PATH`; control with `--cua auto|on|off`):

```bash
agent-box call box1 list_windows
agent-box call box1 get_window_state '{"window_title":"Calculator"}'
agent-box call box1 click '{"x":120,"y":240}'
```

`agent-box call` injects `"session": "<name>"` automatically and adds `"delivery_mode": "foreground"` for pixel/keyboard input calls when no `element_token` or `element_index` is given.

5. Take a full-screen PNG screenshot at any time (native X11 capture, works with `--cua off`):

```bash
agent-box shot box1 /tmp/box1.png
```

6. Always tear down boxes when finished:

```bash
agent-box down box1
# or remove all boxes:
agent-box down --all
```

Run `agent-box gc` if a crash or reboot left stopped box files behind.

## Safety and reliability rules

- **Never click Sign out** inside a signed-in box. Cloned cookies share server-side sessions with the user's main browser, so signing out in a box signs out the host browser too. Run `agent-box down <name>` instead.
- **Prefer CDP over GTK file dialogs** for file uploads (`agent-browser upload` or Playwright `setInputFiles`). Modal GTK dialogs can stall AT-SPI accessibility calls.
- **Handle `call` timeout (exit code `124`)**: If an accessibility read hangs for `--timeout` seconds (default `60`), take `agent-box shot <name>` to check for a modal dialog, then target or dismiss it.
- **No Snap or Flatpak browsers**: `agent-box` requires a native system browser package (`.deb`, `.rpm`, or distro package).
- **Check logs on failure**: Live logs live in `~/.cache/agent-box/<name>/logs/`. Failed startup or crash logs are preserved in `$XDG_RUNTIME_DIR/agent-box-logs/<name>-<ms>/` and `journalctl --user -u agent-box-<name>`.

## Command reference

| Command | Purpose |
| --- | --- |
| `agent-box up [NAME]` | Create box, clone profile (unless `--blank`/`--no-browser`), print JSON |
| `agent-box ls [--json]` | List boxes with state, display, memory (MB), and CDP URL |
| `agent-box cdp NAME` | Print `http://127.0.0.1:<port>` for the box's browser |
| `agent-box open NAME URL` | Open a URL in the box's browser |
| `agent-box run NAME CMD...` | Launch any program inside the box's display and systemd slice |
| `agent-box call NAME TOOL [JSON]` | Call a `cua-driver` tool inside the box (`--shot`, `--timeout`) |
| `agent-box shot NAME [FILE]` | Capture full-screen X11 PNG and print its path |
| `agent-box env NAME` | Print shell `export` lines targeting the box |
| `agent-box gc` | Delete stopped boxes and their cached profile files |
| `agent-box down NAME... \| --all` | Stop boxes and remove their files |
| `agent-box doctor` | Check host dependencies and browser/profile readiness |
