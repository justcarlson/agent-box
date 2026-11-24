//! Browser and profile auto-detection for Chromium-family browsers on Linux.

use crate::paths::{config_home, find_in_paths, is_executable_file};
use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserSpec {
    pub kind: &'static str,
    pub binaries: &'static [&'static str],
    pub data_dir_rel: &'static str,
    pub keyring_apps: &'static [&'static str],
}

/// Unified table of supported Chromium-family browsers.
pub const BROWSERS: &[BrowserSpec] = &[
    BrowserSpec {
        kind: "helium",
        binaries: &["/opt/helium-browser-bin/helium", "helium", "helium-browser"],
        data_dir_rel: "net.imput.helium",
        keyring_apps: &["helium", "chromium"],
    },
    BrowserSpec {
        kind: "chromium",
        binaries: &[
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "chromium",
            "chromium-browser",
        ],
        data_dir_rel: "chromium",
        keyring_apps: &["chromium"],
    },
    BrowserSpec {
        kind: "chrome",
        binaries: &[
            "/opt/google/chrome/chrome",
            "google-chrome-stable",
            "google-chrome",
        ],
        data_dir_rel: "google-chrome",
        keyring_apps: &["chrome", "chromium"],
    },
    BrowserSpec {
        kind: "brave",
        binaries: &["/opt/brave.com/brave/brave", "brave-browser", "brave"],
        data_dir_rel: "BraveSoftware/Brave-Browser",
        keyring_apps: &["brave", "chromium"],
    },
    BrowserSpec {
        kind: "vivaldi",
        binaries: &["/opt/vivaldi/vivaldi", "vivaldi", "vivaldi-stable"],
        data_dir_rel: "vivaldi",
        keyring_apps: &["vivaldi", "chromium"],
    },
    BrowserSpec {
        kind: "edge",
        binaries: &[
            "/opt/microsoft/msedge/msedge",
            "microsoft-edge",
            "microsoft-edge-stable",
        ],
        data_dir_rel: "microsoft-edge",
        keyring_apps: &["microsoft-edge", "chromium"],
    },
];

/// Fallback keyring `application=` candidates when `--from` or `--browser`
/// does not match a known browser kind.
pub const ALL_KEYRING_APPS: &[&str] = &[
    "helium",
    "chrome",
    "brave",
    "vivaldi",
    "microsoft-edge",
    "chromium",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confinement {
    Snap,
    Flatpak,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedBrowser {
    pub kind: &'static str,
    pub binary: PathBuf,
    pub data_dir: PathBuf,
    pub keyring_apps: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBrowser {
    pub kind: Option<&'static str>,
    pub binary: PathBuf,
    pub data_dir: Option<PathBuf>,
    pub keyring_apps: &'static [&'static str],
}

fn path_confinement(path: &Path) -> Option<Confinement> {
    let s = path.to_string_lossy();
    if s.starts_with("/snap/") || s.contains("/snap/") {
        return Some(Confinement::Snap);
    }
    if s.contains("/var/lib/flatpak") || s.contains("/.local/share/flatpak") {
        return Some(Confinement::Flatpak);
    }
    None
}

fn line_invokes(line: &str, tool: &str, subcmd: &str) -> bool {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for pair in tokens.windows(2) {
        let cmd = Path::new(pair[0])
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(pair[0]);
        if cmd == tool && pair[1] == subcmd {
            return true;
        }
    }
    false
}

/// Detect whether `path` is a Snap or Flatpak browser (either by install path,
/// symlink hop target, canonical snap/flatpak binary, or a shell wrapper script
/// invoking `snap run` / `flatpak run`).
pub fn check_confinement(path: &Path) -> Option<Confinement> {
    if let Some(c) = path_confinement(path) {
        return Some(c);
    }
    let mut cur = path.to_path_buf();
    for _ in 0..40 {
        let Ok(target) = fs::read_link(&cur) else {
            break;
        };
        cur = if target.is_absolute() {
            target
        } else {
            cur.parent().unwrap_or(Path::new("/")).join(target)
        };
        if let Some(c) = path_confinement(&cur) {
            return Some(c);
        }
    }
    if let Ok(canon) = fs::canonicalize(path) {
        if let Some(c) = path_confinement(&canon) {
            return Some(c);
        }
        match canon.file_name().and_then(OsStr::to_str) {
            Some("snap") => return Some(Confinement::Snap),
            Some("flatpak") => return Some(Confinement::Flatpak),
            _ => {}
        }
    }
    let mut f = fs::File::open(path).ok()?;
    let mut buf = [0u8; 8192];
    let n = f.read(&mut buf).ok()?;
    let head = &buf[..n];
    // Skip ELF binaries and binary files.
    if head.starts_with(b"\x7fELF") || head.iter().take(512).any(|&b| b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(head);
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line_invokes(line, "snap", "run") || line.contains("/snap/") {
            return Some(Confinement::Snap);
        }
        if line_invokes(line, "flatpak", "run")
            || line.contains("/var/lib/flatpak")
            || line.contains("/.local/share/flatpak")
        {
            return Some(Confinement::Flatpak);
        }
    }
    None
}

pub fn confinement_error(path: &Path, kind: Confinement) -> anyhow::Error {
    let label = match kind {
        Confinement::Snap => "Snap",
        Confinement::Flatpak => "Flatpak",
    };
    anyhow!(
        "{} is a {label}-packaged browser, which is not supported: {label} confinement breaks agent-box systemd slice and private X11/D-Bus isolation. Install a native .deb, .rpm, or system package instead.",
        path.display()
    )
}

/// Validate a user-supplied `--browser` binary path (or command name on PATH).
pub fn validate_binary(input: &Path) -> Result<PathBuf> {
    let resolved = if input.components().count() == 1 && !input.is_absolute() {
        let name = input
            .to_str()
            .with_context(|| format!("invalid browser name {}", input.display()))?;
        find_in_paths(name, env::var_os("PATH").as_deref())
            .with_context(|| format!("browser {name:?} not found on PATH"))?
    } else {
        if !is_executable_file(input) {
            bail!(
                "browser binary is missing or not executable: {}",
                input.display()
            );
        }
        input.to_path_buf()
    };
    if let Some(c) = check_confinement(&resolved) {
        return Err(confinement_error(&resolved, c));
    }
    Ok(resolved)
}

/// Find an installed native binary for `spec`. If only a Snap/Flatpak binary is
/// installed for `spec`, returns an `Err` explaining the confinement problem.
pub(crate) fn find_binary_for_with(
    spec: &BrowserSpec,
    path_var: Option<&OsStr>,
) -> Result<Option<PathBuf>> {
    let mut confined: Option<(PathBuf, Confinement)> = None;
    for candidate in spec.binaries {
        let p = Path::new(candidate);
        let found = if p.is_absolute() {
            is_executable_file(p).then(|| p.to_path_buf())
        } else {
            find_in_paths(candidate, path_var)
        };
        if let Some(bin) = found {
            if let Some(c) = check_confinement(&bin) {
                if confined.is_none() {
                    confined = Some((bin, c));
                }
                continue;
            }
            return Ok(Some(bin));
        }
    }
    if let Some((bin, c)) = confined {
        return Err(confinement_error(&bin, c));
    }
    Ok(None)
}

/// Infer a browser specification from a `--from` data directory path.
pub fn infer_from_data_dir(path: &Path) -> Option<&'static BrowserSpec> {
    let mut candidates = vec![path.to_path_buf()];
    if let Ok(canon) = fs::canonicalize(path) {
        if canon != path {
            candidates.push(canon);
        }
    }
    for p in &candidates {
        for spec in BROWSERS {
            if p.ends_with(Path::new(spec.data_dir_rel)) {
                return Some(spec);
            }
        }
    }
    for p in &candidates {
        let last = p.file_name()?.to_string_lossy().to_ascii_lowercase();
        let matched = match last.as_str() {
            "net.imput.helium" | "helium" => Some("helium"),
            "chromium" => Some("chromium"),
            "google-chrome" | "google-chrome-beta" | "google-chrome-unstable" | "chrome" => {
                Some("chrome")
            }
            "brave-browser" | "brave-browser-beta" | "brave-browser-nightly" | "brave" => {
                Some("brave")
            }
            "vivaldi" | "vivaldi-snapshot" => Some("vivaldi"),
            "microsoft-edge" | "microsoft-edge-beta" | "microsoft-edge-dev" | "edge" => {
                Some("edge")
            }
            _ => None,
        };
        if let Some(kind) = matched {
            return BROWSERS.iter().find(|s| s.kind == kind);
        }
    }
    None
}

/// Infer a browser specification from a binary path or name.
pub fn infer_from_binary(path: &Path) -> Option<&'static BrowserSpec> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    for spec in BROWSERS {
        if name == spec.kind
            || spec.binaries.iter().any(|b| {
                Path::new(b)
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|bn| bn.eq_ignore_ascii_case(&name))
            })
        {
            return Some(spec);
        }
    }
    if name.contains("helium") {
        BROWSERS.iter().find(|s| s.kind == "helium")
    } else if name.contains("chromium") {
        BROWSERS.iter().find(|s| s.kind == "chromium")
    } else if name.contains("chrome") {
        BROWSERS.iter().find(|s| s.kind == "chrome")
    } else if name.contains("brave") {
        BROWSERS.iter().find(|s| s.kind == "brave")
    } else if name.contains("vivaldi") {
        BROWSERS.iter().find(|s| s.kind == "vivaldi")
    } else if name.contains("edge") || name.contains("msedge") {
        BROWSERS.iter().find(|s| s.kind == "edge")
    } else {
        None
    }
}

/// Auto-detect the best installed Chromium-family browser: the installed native
/// browser whose `<data_dir>/Local State` was modified most recently, falling
/// back to the first installed native browser in `BROWSERS`.
pub fn detect() -> Result<DetectedBrowser> {
    detect_with(&config_home(), env::var_os("PATH").as_deref(), BROWSERS)
}

pub(crate) fn detect_with(
    cfg_home: &Path,
    path_var: Option<&OsStr>,
    specs: &[BrowserSpec],
) -> Result<DetectedBrowser> {
    let mut best: Option<(DetectedBrowser, Option<SystemTime>)> = None;
    let mut first_confinement_err: Option<anyhow::Error> = None;

    for spec in specs {
        let binary = match find_binary_for_with(spec, path_var) {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => {
                if first_confinement_err.is_none() {
                    first_confinement_err = Some(e);
                }
                continue;
            }
        };
        let data_dir = cfg_home.join(spec.data_dir_rel);
        let mtime = fs::metadata(data_dir.join("Local State"))
            .and_then(|m| m.modified())
            .ok();
        let candidate = DetectedBrowser {
            kind: spec.kind,
            binary,
            data_dir,
            keyring_apps: spec.keyring_apps,
        };
        let replace = match (&best, mtime) {
            (None, _) => true,
            (Some((_, None)), Some(_)) => true,
            (Some((_, Some(prev))), Some(cur)) => cur > *prev,
            _ => false,
        };
        if replace {
            best = Some((candidate, mtime));
        }
    }

    if let Some((det, _)) = best {
        return Ok(det);
    }
    if let Some(err) = first_confinement_err {
        return Err(err);
    }
    bail!(
        "no supported Chromium-family browser found (checked helium, chromium, chrome, brave, vivaldi, edge); install one or pass --browser / --no-browser"
    )
}

/// Resolve the browser binary, profile source directory, and keyring app
/// candidates for `agent-box up`.
pub fn resolve(
    browser_arg: Option<&Path>,
    from_arg: Option<&Path>,
    blank: bool,
) -> Result<ResolvedBrowser> {
    resolve_with(
        browser_arg,
        from_arg,
        blank,
        &config_home(),
        env::var_os("PATH").as_deref(),
        BROWSERS,
    )
}

pub(crate) fn resolve_with(
    browser_arg: Option<&Path>,
    from_arg: Option<&Path>,
    blank: bool,
    cfg_home: &Path,
    path_var: Option<&OsStr>,
    specs: &[BrowserSpec],
) -> Result<ResolvedBrowser> {
    if let Some(bin_arg) = browser_arg {
        let binary = if bin_arg.components().count() == 1 && !bin_arg.is_absolute() {
            let name = bin_arg
                .to_str()
                .with_context(|| format!("invalid browser name {}", bin_arg.display()))?;
            let p = find_in_paths(name, path_var)
                .with_context(|| format!("browser {name:?} not found on PATH"))?;
            if let Some(c) = check_confinement(&p) {
                return Err(confinement_error(&p, c));
            }
            p
        } else {
            validate_binary(bin_arg)?
        };
        let spec = from_arg
            .and_then(infer_from_data_dir)
            .or_else(|| infer_from_binary(&binary));
        let data_dir = if blank {
            None
        } else {
            from_arg
                .map(PathBuf::from)
                .or_else(|| spec.map(|s| cfg_home.join(s.data_dir_rel)))
                .or_else(|| {
                    detect_with(cfg_home, path_var, specs)
                        .ok()
                        .map(|d| d.data_dir)
                })
        };
        return Ok(ResolvedBrowser {
            kind: spec.map(|s| s.kind),
            binary,
            data_dir,
            keyring_apps: spec.map(|s| s.keyring_apps).unwrap_or(ALL_KEYRING_APPS),
        });
    }

    if let Some(src) = from_arg {
        if let Some(spec) = infer_from_data_dir(src) {
            let matching_spec = specs.iter().find(|s| s.kind == spec.kind).unwrap_or(spec);
            let binary = find_binary_for_with(matching_spec, path_var)?.with_context(|| {
                format!(
                    "profile at {} matches {}, but no native {} binary was found; install it or pass --browser",
                    src.display(),
                    matching_spec.kind,
                    matching_spec.kind
                )
            })?;
            return Ok(ResolvedBrowser {
                kind: Some(matching_spec.kind),
                binary,
                data_dir: (!blank).then(|| src.to_path_buf()),
                keyring_apps: matching_spec.keyring_apps,
            });
        }
        let det = detect_with(cfg_home, path_var, specs)?;
        return Ok(ResolvedBrowser {
            kind: Some(det.kind),
            binary: det.binary,
            data_dir: (!blank).then(|| src.to_path_buf()),
            keyring_apps: ALL_KEYRING_APPS,
        });
    }

    let det = detect_with(cfg_home, path_var, specs)?;
    Ok(ResolvedBrowser {
        kind: Some(det.kind),
        binary: det.binary,
        data_dir: (!blank).then_some(det.data_dir),
        keyring_apps: det.keyring_apps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "agent-box-browser-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_exec(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn table_has_all_six_browsers_and_expected_metadata() {
        let kinds: Vec<&str> = BROWSERS.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec!["helium", "chromium", "chrome", "brave", "vivaldi", "edge"]
        );
        assert_eq!(BROWSERS[0].data_dir_rel, "net.imput.helium");
        assert_eq!(BROWSERS[0].keyring_apps, &["helium", "chromium"]);
        assert_eq!(BROWSERS[1].data_dir_rel, "chromium");
        assert_eq!(BROWSERS[1].keyring_apps, &["chromium"]);
        assert_eq!(BROWSERS[2].data_dir_rel, "google-chrome");
        assert_eq!(BROWSERS[2].keyring_apps, &["chrome", "chromium"]);
        assert_eq!(BROWSERS[3].data_dir_rel, "BraveSoftware/Brave-Browser");
        assert_eq!(BROWSERS[3].keyring_apps, &["brave", "chromium"]);
        assert_eq!(BROWSERS[4].data_dir_rel, "vivaldi");
        assert_eq!(BROWSERS[4].keyring_apps, &["vivaldi", "chromium"]);
        assert_eq!(BROWSERS[5].data_dir_rel, "microsoft-edge");
        assert_eq!(BROWSERS[5].keyring_apps, &["microsoft-edge", "chromium"]);
    }

    #[test]
    fn auto_detect_picks_most_recently_modified_local_state() {
        let root = temp_dir("mtime");
        let bin_dir = root.join("bin");
        let cfg = root.join("config");
        make_exec(&bin_dir.join("fake-chromium"), "#!/bin/sh\nexit 0\n");
        make_exec(&bin_dir.join("fake-chrome"), "#!/bin/sh\nexit 0\n");

        let specs = [
            BrowserSpec {
                kind: "chromium",
                binaries: &["fake-chromium"],
                data_dir_rel: "chromium",
                keyring_apps: &["chromium"],
            },
            BrowserSpec {
                kind: "chrome",
                binaries: &["fake-chrome"],
                data_dir_rel: "google-chrome",
                keyring_apps: &["chrome", "chromium"],
            },
        ];

        // Neither has Local State -> falls back to first installed (chromium).
        let det = detect_with(&cfg, Some(bin_dir.as_os_str()), &specs).unwrap();
        assert_eq!(det.kind, "chromium");

        // Create Local State for chromium first, then sleep briefly and create for chrome.
        fs::create_dir_all(cfg.join("chromium")).unwrap();
        fs::write(cfg.join("chromium/Local State"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::create_dir_all(cfg.join("google-chrome")).unwrap();
        fs::write(cfg.join("google-chrome/Local State"), "{}").unwrap();

        let det = detect_with(&cfg, Some(bin_dir.as_os_str()), &specs).unwrap();
        assert_eq!(det.kind, "chrome");
        assert_eq!(det.data_dir, cfg.join("google-chrome"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn infers_browser_from_from_path_and_pairs_with_binary() {
        let root = temp_dir("infer");
        let bin_dir = root.join("bin");
        let cfg = root.join("config");
        make_exec(&bin_dir.join("brave-browser"), "#!/bin/sh\nexit 0\n");
        make_exec(&bin_dir.join("chromium"), "#!/bin/sh\nexit 0\n");

        let specs = [
            BrowserSpec {
                kind: "chromium",
                binaries: &["chromium"],
                data_dir_rel: "chromium",
                keyring_apps: &["chromium"],
            },
            BrowserSpec {
                kind: "brave",
                binaries: &["brave-browser"],
                data_dir_rel: "BraveSoftware/Brave-Browser",
                keyring_apps: &["brave", "chromium"],
            },
        ];

        let from = cfg.join("BraveSoftware/Brave-Browser");
        fs::create_dir_all(&from).unwrap();
        let res = resolve_with(
            None,
            Some(&from),
            false,
            &cfg,
            Some(bin_dir.as_os_str()),
            &specs,
        )
        .unwrap();
        assert_eq!(res.kind, Some("brave"));
        assert_eq!(res.binary, bin_dir.join("brave-browser"));
        assert_eq!(res.data_dir.as_deref(), Some(from.as_path()));
        assert_eq!(res.keyring_apps, &["brave", "chromium"]);

        assert_eq!(
            infer_from_data_dir(Path::new("/home/u/.config/net.imput.helium")).map(|s| s.kind),
            Some("helium")
        );
        assert_eq!(
            infer_from_data_dir(Path::new("/home/u/.config/microsoft-edge")).map(|s| s.kind),
            Some("edge")
        );
        assert_eq!(
            infer_from_data_dir(Path::new("/home/u/.config/vivaldi")).map(|s| s.kind),
            Some("vivaldi")
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn detects_snap_and_flatpak_paths_and_wrappers() {
        assert_eq!(
            path_confinement(Path::new("/snap/bin/chromium")),
            Some(Confinement::Snap)
        );
        assert_eq!(
            path_confinement(Path::new("/var/lib/flatpak/exports/bin/com.brave.Browser")),
            Some(Confinement::Flatpak)
        );
        assert_eq!(
            path_confinement(Path::new(
                "/home/u/.local/share/flatpak/exports/bin/org.chromium.Chromium"
            )),
            Some(Confinement::Flatpak)
        );
        assert_eq!(path_confinement(Path::new("/usr/bin/chromium")), None);

        let root = temp_dir("confinement");
        let snap_wrap = root.join("chromium-snap");
        make_exec(
            &snap_wrap,
            "#!/bin/sh\nexec /usr/bin/snap run chromium \"$@\"\n",
        );
        assert_eq!(check_confinement(&snap_wrap), Some(Confinement::Snap));
        let err = validate_binary(&snap_wrap).unwrap_err().to_string();
        assert!(err.contains("Snap"), "{err}");
        assert!(err.contains(".deb"), "{err}");

        let fp_wrap = root.join("chrome-flatpak");
        make_exec(
            &fp_wrap,
            "#!/bin/sh\nflatpak run com.google.Chrome \"$@\"\n",
        );
        assert_eq!(check_confinement(&fp_wrap), Some(Confinement::Flatpak));
        let err = validate_binary(&fp_wrap).unwrap_err().to_string();
        assert!(err.contains("Flatpak"), "{err}");

        let native = root.join("native-elf");
        make_exec(&native, "\x7fELFsnap run flatpak run");
        assert_eq!(check_confinement(&native), None);
        assert!(validate_binary(&native).is_ok());

        // Auto-detect fails cleanly when only a Snap wrapper is installed.
        let bin_dir = root.join("snap-only-bin");
        make_exec(
            &bin_dir.join("chromium-browser"),
            "#!/bin/sh\nsnap run chromium \"$@\"\n",
        );
        let specs = [BrowserSpec {
            kind: "chromium",
            binaries: &["chromium-browser"],
            data_dir_rel: "chromium",
            keyring_apps: &["chromium"],
        }];
        let err = detect_with(&root.join("cfg"), Some(bin_dir.as_os_str()), &specs)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Snap"), "{err}");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_handles_explicit_browser_and_custom_from_and_blank() {
        let root = temp_dir("resolve");
        let bin_dir = root.join("bin");
        let cfg = root.join("config");
        make_exec(&bin_dir.join("chromium"), "#!/bin/sh\nexit 0\n");

        let specs = [BrowserSpec {
            kind: "chromium",
            binaries: &["chromium"],
            data_dir_rel: "chromium",
            keyring_apps: &["chromium"],
        }];

        // Explicit bare browser name resolves on PATH and infers its data dir.
        let res = resolve_with(
            Some(Path::new("chromium")),
            None,
            false,
            &cfg,
            Some(bin_dir.as_os_str()),
            &specs,
        )
        .unwrap();
        assert_eq!(res.kind, Some("chromium"));
        assert_eq!(res.binary, bin_dir.join("chromium"));
        assert_eq!(res.data_dir, Some(cfg.join("chromium")));
        assert_eq!(res.keyring_apps, &["chromium"]);

        // Blank omits data_dir.
        let res = resolve_with(None, None, true, &cfg, Some(bin_dir.as_os_str()), &specs).unwrap();
        assert_eq!(res.data_dir, None);

        // Custom unrecognized --from keeps custom dir and falls back to ALL_KEYRING_APPS.
        let custom = root.join("my-custom-profile");
        let res = resolve_with(
            None,
            Some(&custom),
            false,
            &cfg,
            Some(bin_dir.as_os_str()),
            &specs,
        )
        .unwrap();
        assert_eq!(res.data_dir.as_deref(), Some(custom.as_path()));
        assert_eq!(res.keyring_apps, ALL_KEYRING_APPS);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn symlink_chain_to_snap_binary_is_detected() {
        let root = std::env::temp_dir().join(format!("agent-box-snap-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let snap_bin = root.join("usr/bin/snap");
        make_exec(&snap_bin, "\x7fELFdummy");
        let hop = root.join("snap/bin/chromium");
        fs::create_dir_all(hop.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&snap_bin, &hop).unwrap();
        let wrapper = root.join("home-bin/chromium");
        fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&hop, &wrapper).unwrap();

        assert_eq!(check_confinement(&wrapper), Some(Confinement::Snap));
        fs::remove_dir_all(&root).unwrap();
    }
}
