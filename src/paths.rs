//! Where each box keeps its files, and the environment its apps see.

use anyhow::{bail, Result};
use std::env;
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").expect("HOME is set"))
}

fn state_root() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"))
        .join("agent-box")
}

fn runtime_root() -> PathBuf {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    base.join("agent-box")
}

pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 24
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.as_bytes()[0].is_ascii_lowercase();
    if !ok {
        bail!("box name must be 1-24 lowercase letters or digits, starting with a letter: {name:?}");
    }
    Ok(())
}

pub fn all_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(state_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| check_name(n).is_ok())
        .collect();
    names.sort();
    names
}

pub struct Box {
    pub name: String,
    /// Persistent per-box files: HOME, browser profile, logs, screenshots.
    pub dir: PathBuf,
    /// Sockets and readiness markers (tmpfs).
    pub run: PathBuf,
}

impl Box {
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
        std::fs::read_to_string(self.ready_file()).ok()?.trim().parse().ok()
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
            ("DBUS_SESSION_BUS_ADDRESS".into(), format!("unix:path={}", self.bus().display())),
            ("GDK_BACKEND".into(), "x11".into()),
            ("QT_QPA_PLATFORM".into(), "xcb".into()),
            ("GTK_USE_PORTAL".into(), "0".into()),
            ("AGENT_BOX".into(), self.name.clone()),
        ];
        for key in ["PATH", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TZ", "TERM"] {
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
    }
}
