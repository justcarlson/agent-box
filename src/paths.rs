//! Where each box keeps its files, and the environment its apps see.

use anyhow::{bail, Context, Result};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Standard Cache Directory Tagging Specification header so backup tools skip
/// cloned browser state.
pub const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# Created by agent-box; contains temporary cloned browser state.\n";

pub fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").expect("HOME is set"))
}

pub fn config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

fn state_root() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("agent-box")
}

fn runtime_root() -> PathBuf {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    base.join("agent-box")
}

/// Create `~/.cache/agent-box` (owner-only) and ensure `CACHEDIR.TAG` exists.
pub fn ensure_state_root() -> Result<PathBuf> {
    let state = state_root();
    ensure_state_root_at(&state)?;
    Ok(state)
}

pub(crate) fn ensure_state_root_at(state: &Path) -> Result<()> {
    fs::create_dir_all(state).with_context(|| format!("create {}", state.display()))?;
    // Boxes hold cookie copies: owner only.
    fs::set_permissions(state, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", state.display()))?;
    let tag = state.join("CACHEDIR.TAG");
    if !tag.exists() {
        fs::write(&tag, CACHEDIR_TAG).with_context(|| format!("write {}", tag.display()))?;
    }
    Ok(())
}

/// True if `path` exists as a regular file (following symlinks) with any execute bit set.
pub fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
}

/// Locate an executable binary on `$PATH`.
pub fn find_on_path(bin: &str) -> Option<PathBuf> {
    find_in_paths(bin, env::var_os("PATH").as_deref())
}

pub fn find_in_paths(bin: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    for dir in env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 24
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.as_bytes()[0].is_ascii_lowercase();
    if !ok {
        bail!(
            "box name must be 1-24 lowercase letters or digits, starting with a letter: {name:?}"
        );
    }
    Ok(())
}

pub fn all_names() -> Vec<String> {
    all_names_in(&state_root())
}

pub(crate) fn all_names_in(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| check_name(n).is_ok())
        .collect();
    names.sort();
    names
}

pub struct AgentBox {
    pub name: String,
    /// Persistent per-box files: HOME, browser profile, logs, screenshots.
    pub dir: PathBuf,
    /// Sockets and readiness markers (tmpfs).
    pub run: PathBuf,
}

impl AgentBox {
    pub fn new(name: &str) -> Result<Self> {
        check_name(name)?;
        Ok(Self {
            name: name.to_string(),
            dir: state_root().join(name),
            run: runtime_root().join(name),
        })
    }
    pub fn home(&self) -> PathBuf {
        self.dir.join("home")
    }
    pub fn browser_dir(&self) -> PathBuf {
        self.home().join("browser")
    }
    pub fn bus(&self) -> PathBuf {
        self.run.join("bus")
    }
    pub fn cua_socket(&self) -> PathBuf {
        self.run.join("cua.sock")
    }
    pub fn xauth(&self) -> PathBuf {
        self.run.join("xauth")
    }
    pub fn ready_file(&self) -> PathBuf {
        self.run.join("ready")
    }
    pub fn meta_file(&self) -> PathBuf {
        self.dir.join("box.json")
    }
    pub fn unit(&self) -> String {
        format!("agent-box-{}", self.name)
    }
    pub fn slice(&self) -> String {
        format!("agent-box-{}.slice", self.name)
    }
    /// X display number, once the stack is ready.
    pub fn display(&self) -> Option<u32> {
        fs::read_to_string(self.ready_file())
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The whole environment for processes inside the box. Everything else
    /// from the caller is dropped, so no app reaches the user's Wayland
    /// session, session bus or keyring.
    pub fn app_env(&self, display: u32) -> Vec<(String, String)> {
        let home = self.home();
        let h = |p: &str| home.join(p).display().to_string();
        let mut env = vec![
            ("HOME".into(), home.display().to_string()),
            ("XDG_CONFIG_HOME".into(), h(".config")),
            ("XDG_CACHE_HOME".into(), h(".cache")),
            ("XDG_DATA_HOME".into(), h(".local/share")),
            ("XDG_STATE_HOME".into(), h(".local/state")),
            ("XDG_RUNTIME_DIR".into(), self.run.display().to_string()),
            ("XDG_SESSION_TYPE".into(), "x11".into()),
            ("DISPLAY".into(), format!(":{display}")),
            ("XAUTHORITY".into(), self.xauth().display().to_string()),
            (
                "DBUS_SESSION_BUS_ADDRESS".into(),
                format!("unix:path={}", self.bus().display()),
            ),
            ("GDK_BACKEND".into(), "x11".into()),
            ("QT_QPA_PLATFORM".into(), "xcb".into()),
            ("GTK_USE_PORTAL".into(), "0".into()),
            ("AGENT_BOX".into(), self.name.clone()),
        ];
        for key in [
            "PATH", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TZ", "TERM",
        ] {
            if let Ok(v) = env::var(key) {
                env.push((key.into(), v));
            }
        }
        env
    }
}

pub fn is_inside(child: &Path, root: &Path) -> bool {
    child.starts_with(root) && child != root
}

pub fn roots() -> (PathBuf, PathBuf) {
    (state_root(), runtime_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(check_name("box1").is_ok());
        assert!(check_name("slack").is_ok());
        assert!(check_name("1box").is_err());
        assert!(check_name("a-b").is_err());
        assert!(check_name("../x").is_err());
        assert!(check_name("").is_err());
        assert!(check_name("CACHEDIR.TAG").is_err());
    }

    #[test]
    fn state_root_writes_cachedir_tag_and_mode_0700() {
        let dir = env::temp_dir().join(format!("agent-box-cachedir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ensure_state_root_at(&dir).unwrap();
        let tag = fs::read_to_string(dir.join("CACHEDIR.TAG")).unwrap();
        assert_eq!(tag, CACHEDIR_TAG);
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(all_names_in(&dir).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
