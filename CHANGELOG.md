# Changelog

All notable changes to `agent-box` are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

## 0.1.0 - 2026-11-23

### Added

- Parallel, disposable Linux desktops isolated per systemd user slice (`Xvfb` on `:100+`, private `dbus-daemon`, AT-SPI bus/registry, `openbox`, and optional `cua-driver`).
- Auto-detection for native Linux installs of Helium, Chromium, Google Chrome, Brave, Vivaldi, and Microsoft Edge, with Snap and Flatpak confinement checks.
- Copy-on-write (`FICLONE`) profile cloning with SQLite rollback-journal consistency checks and in-memory `v11` to `v10` cookie re-encryption via non-prompting `secret-tool search`.
- Privacy-safe defaults: `0700` state directory, `CACHEDIR.TAG`, `--disable-sync`, skipped caches/history/saved passwords/extensions (opt-in `--with-extensions`), and `ExecStopPost` automatic reaping plus `agent-box gc`.
- Pure-Rust X11 root screenshot capture (`x11rb` + `png`) with zero external screenshot tool dependencies.
- Optional `cua-driver` integration (`--cua auto|on|off`) and direct Chrome DevTools Protocol (`agent-box cdp`) support.
- Distro-aware `agent-box doctor` preflight checks for Arch, Debian/Ubuntu, and Fedora.
- Bundled AI agent skill (`skills/agent-box/SKILL.md`) and parallel smoke demo (`examples/parallel-demo.sh`).
