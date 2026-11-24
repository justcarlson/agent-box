//! agent-box: parallel, disposable desktops for computer-use agents.
//!
//! Each box is a private X display with its own D-Bus, accessibility bus,
//! window manager and cua-driver daemon, in its own systemd slice. Its
//! browser starts from a copy-on-write clone of the user's main profile, so
//! it is signed in wherever the user is. Nothing a box does reaches the
//! user's desktop or writes back to the user's profile.

mod paths;
mod profile;
mod stack;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use paths::Box;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(name = "agent-box", version, about = "Parallel, disposable computer-use desktops signed in like your main browser")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a box and open its browser. Prints JSON with the box's handles.
    Up {
        /// Box name (lowercase letters and digits). Default: the first free boxN.
        name: Option<String>,
        /// URL for the first tab.
        #[arg(long, default_value = "about:blank")]
        url: String,
        /// Do not start a browser (use `run` for other apps).
        #[arg(long)]
        no_browser: bool,
        /// Start the browser with an empty profile instead of a clone.
        #[arg(long)]
        blank: bool,
        /// Browser data directory to clone.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Profile directory inside --from. Default: the one last used.
        #[arg(long)]
        profile: Option<String>,
        /// Screen size, WIDTHxHEIGHT.
        #[arg(long, default_value = "1440x900")]
        size: String,
        /// Soft memory limit for the whole box (systemd MemoryHigh).
        #[arg(long, default_value = "3G")]
        mem: String,
        /// Browser binary. Must match the cloned profile's browser.
        #[arg(long)]
        browser: Option<PathBuf>,
        /// Stop the box after this long, in case nobody runs `down`.
        #[arg(long, default_value = "12h")]
        ttl: String,
    },
    /// Stop boxes and delete their files.
    Down {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// List boxes.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Call a cua-driver tool in a box. Adds "session":NAME, and foreground
    /// delivery for pixel input, when the JSON does not set them.
    Call {
        name: String,
        tool: String,
        /// JSON arguments, or `-` to read them from stdin. Default `{}`.
        json: Option<String>,
        /// Save the first image in the result to this file.
        #[arg(long)]
        shot: Option<PathBuf>,
        /// Give up after this many seconds. A modal dialog can stall some
        /// accessibility reads.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Start any program inside a box.
    Run {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open a URL in the box's browser, starting the browser if needed.
    Open { name: String, url: String },
    /// Save a PNG of the box's whole screen and print its path.
    Shot { name: String, file: Option<PathBuf> },
    /// Print the box browser's Chrome DevTools Protocol endpoint.
    Cdp { name: String },
    /// Print `export` lines for running commands inside a box.
    Env { name: String },
    /// Check that this host has everything agent-box needs.
    Doctor,
    #[command(hide = true, name = "__reap")]
    Reap { name: String },
    #[command(hide = true, name = "__stack")]
    Stack {
        name: String,
        #[arg(long)]
        size: String,
    },
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agent-box: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `agent-box env` saves each value it changes as `AGENT_BOX_HOST_<KEY>`
/// ("" when it was unset). Put them back, so a shell that ran
/// `eval "$(agent-box env NAME)"` still manages the host's boxes.
fn restore_host_env() {
    let saved: Vec<(String, String)> = std::env::vars()
        .filter_map(|(k, v)| Some((k.strip_prefix("AGENT_BOX_HOST_")?.to_string(), v)))
        .collect();
    for (k, v) in saved {
        if v.is_empty() {
            std::env::remove_var(&k);
        } else {
            std::env::set_var(&k, v);
        }
    }
}

fn real_main() -> Result<ExitCode> {
    // Before any thread starts, so changing the environment is safe.
    restore_host_env();
    match Cli::parse().cmd {
        Cmd::Up { name, url, no_browser, blank, from, profile, size, mem, browser, ttl } => {
            up(UpArgs { name, url, no_browser, blank, from, profile, size, mem, browser, ttl })?
        }
        Cmd::Down { names, all } => down(names, all)?,
        Cmd::Ls { json } => ls(json)?,
        Cmd::Call { name, tool, json, shot, timeout } => return call(&name, &tool, json, shot, timeout),
        Cmd::Run { name, cmd } => {
            let b = existing(&name)?;
            let unit = start_app(&b, "app", &cmd)?;
            println!("{unit}");
        }
        Cmd::Open { name, url } => {
            let b = existing(&name)?;
            start_browser(&b, &url)?;
        }
        Cmd::Shot { name, file } => println!("{}", shot(&existing(&name)?, file)?.display()),
        Cmd::Cdp { name } => println!("{}", cdp_url(&existing(&name)?)?),
        Cmd::Env { name } => {
            let b = existing(&name)?;
            let vars = b.app_env(ready_display(&b)?);
            for (k, _) in &vars {
                let host = std::env::var(k).unwrap_or_default();
                println!("export AGENT_BOX_HOST_{k}={}", shell_quote(&host));
            }
            for (k, v) in &vars {
                println!("export {k}={}", shell_quote(v));
            }
        }
        Cmd::Doctor => return doctor(),
        Cmd::Reap { name } => reap_stopped(&Box::new(&name)?)?,
        Cmd::Stack { name, size } => {
            let (w, h) = parse_size(&size)?;
            stack::run(&Box::new(&name)?, w, h)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

struct UpArgs {
    name: Option<String>,
    url: String,
    no_browser: bool,
    blank: bool,
    from: Option<PathBuf>,
    profile: Option<String>,
    size: String,
    mem: String,
    browser: Option<PathBuf>,
    ttl: String,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("size must be WIDTHxHEIGHT")?;
    Ok((w.parse()?, h.parse()?))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A token unique to one creation of a box, so a late cleanup can tell a
/// replacement box of the same name from its own.
fn generation() -> String {
    format!("{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos())
}

fn read_gen(b: &Box) -> Option<String> {
    fs::read_to_string(b.dir.join("gen")).ok()
}

/// Signal that interrupted `up`, if any. `up` checks it between steps and
/// cleans up, so an interrupt never leaves cookie copies behind.
static UP_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn note_up_signal(sig: libc::c_int) {
    UP_SIGNAL.store(sig, std::sync::atomic::Ordering::SeqCst);
}

fn check_interrupt() -> Result<()> {
    match UP_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) {
        0 => Ok(()),
        sig => bail!("interrupted by signal {sig}"),
    }
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

fn existing(name: &str) -> Result<Box> {
    let b = Box::new(name)?;
    if !b.dir.is_dir() {
        bail!("no box named {name}; see `agent-box ls`");
    }
    Ok(b)
}

fn ready_display(b: &Box) -> Result<u32> {
    b.display()
        .filter(|_| unit_state(&b.unit()).as_deref() == Some("active"))
        .with_context(|| format!("box {} is not running; see `agent-box ls` and {}/logs", b.name, b.dir.display()))
}

/// Claim a box name atomically, so parallel `up` calls never share one.
/// The name lock is taken before the box directory exists and is held until
/// `up` ends, so no cleanup deletes a box that is still being created.
fn claim(name: Option<String>) -> Result<(Box, fs::File)> {
    let (state, _) = paths::roots();
    fs::create_dir_all(&state)?;
    // Boxes hold cookie copies: owner only.
    fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let try_claim = |n: &str| -> Result<Option<(Box, fs::File)>> {
        let b = Box::new(n)?;
        if b.dir.exists() {
            return Ok(None);
        }
        let Some(lock) = lock_name(&b, Duration::ZERO)? else { return Ok(None) };
        // An earlier box of this name may still be stopping.
        if !is_stopped(unit_state(&b.unit()).as_deref()) || !is_stopped(unit_state(&b.slice()).as_deref()) {
            return Ok(None);
        }
        match fs::create_dir(&b.dir) {
            Ok(()) => {
                fs::write(b.dir.join("gen"), generation())?;
                Ok(Some((b, lock)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    match name {
        Some(n) => try_claim(&n)?
            .with_context(|| format!("box {n} already exists or is still stopping; see `agent-box ls`")),
        None => {
            // Never hand out an auto-name used before (its lock file stays),
            // so an agent's old name cannot point to another agent's box.
            // Creating the lock file with O_EXCL reserves the name atomically.
            let locks = state.join(".locks");
            fs::DirBuilder::new().recursive(true).mode(0o700).create(&locks)?;
            for i in 1u32.. {
                let n = format!("box{i}");
                match fs::OpenOptions::new().write(true).create_new(true).open(locks.join(&n)) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e.into()),
                }
                if let Some(c) = try_claim(&n)? {
                    return Ok(c);
                }
            }
            bail!("no free box name")
        }
    }
}

/// Take the lock for a box name, waiting up to `wait`. `None` means another
/// process holds it. The lock file lives outside the box directory, so it
/// outlives the box. Whoever holds the lock owns the box's files: `up` while
/// it creates them, and a cleanup while it deletes them.
fn lock_name(b: &Box, wait: Duration) -> Result<Option<fs::File>> {
    use std::os::unix::io::AsRawFd;
    let dir = paths::roots().0.join(".locks");
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let path = dir.join(&b.name);
    let f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(f));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(Duration::from_millis(20));
    }
}

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    // Own process group, so a second Ctrl-C cannot kill a cleanup `stop`.
    Ok(Command::new("systemctl").arg("--user").args(args).process_group(0).output()?)
}

/// `systemctl is-active` output, or `None` when the user manager gave no
/// answer. Callers treat `None` as "unknown", never as stopped.
fn unit_state(unit: &str) -> Option<String> {
    let o = systemctl(&["is-active", unit]).ok()?;
    parse_state(&String::from_utf8_lossy(&o.stdout))
}

fn parse_state(out: &str) -> Option<String> {
    let s = out.trim();
    const KNOWN: &[&str] = &["active", "reloading", "inactive", "failed", "activating", "deactivating", "maintenance", "refreshing"];
    KNOWN.contains(&s).then(|| s.to_string())
}

fn is_stopped(state: Option<&str>) -> bool {
    matches!(state, Some("inactive" | "failed"))
}

fn user_manager_ok() -> bool {
    systemctl(&["is-system-running"]).is_ok_and(|o| {
        matches!(String::from_utf8_lossy(&o.stdout).trim(), "running" | "degraded" | "starting" | "initializing")
    })
}

/// Box names from state directories and from any leftover slices.
fn all_boxes() -> Vec<String> {
    let mut names = paths::all_names();
    if let Ok(o) = systemctl(&["list-units", "--all", "--plain", "--no-legend", "agent-box-*.slice"]) {
        names.extend(slice_names(&String::from_utf8_lossy(&o.stdout)));
    }
    names.sort();
    names.dedup();
    names
}

fn slice_names(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next()?.strip_prefix("agent-box-")?.strip_suffix(".slice").map(String::from))
        .filter(|n| paths::check_name(n).is_ok())
        .collect()
}

/// Environment that `agent-box` itself needs to find the box's paths when
/// systemd starts it with an otherwise empty environment.
fn self_env() -> Vec<String> {
    ["HOME", "PATH", "XDG_RUNTIME_DIR", "XDG_CACHE_HOME", "USER", "LOGNAME", "LANG", "TZ"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| format!("{k}={v}")))
        .collect()
}

fn up(a: UpArgs) -> Result<()> {
    let t0 = Instant::now();
    let (w, h) = parse_size(&a.size)?;
    let browser = match &a.browser {
        Some(p) => Some(p.clone()),
        None if a.no_browser => None,
        None => Some(default_browser().context("no browser found; pass --browser or --no-browser")?),
    };
    if !user_manager_ok() {
        bail!("cannot reach the systemd user manager (systemctl --user)");
    }
    reap_dead();
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic. A signal the caller
        // ignores (nohup, `&`) stays ignored.
        unsafe {
            if libc::signal(sig, note_up_signal as *const () as libc::sighandler_t) == libc::SIG_IGN {
                libc::signal(sig, libc::SIG_IGN);
            }
        }
    }
    let (b, lock) = claim(a.name)?;
    let my_gen = read_gen(&b);
    let result = (|| -> Result<Value> {
        let mut meta = Map::new();
        meta.insert("name".into(), json!(b.name));
        meta.insert("created_ms".into(), json!(now_ms()));
        if let Some(browser) = &browser {
            meta.insert("browser".into(), json!(browser));
            if a.blank {
                meta.insert("profile".into(), json!("blank"));
            } else {
                let src = a.from.clone().unwrap_or_else(profile::default_source);
                let prof = match &a.profile {
                    Some(p) => p.clone(),
                    None => last_used_profile(&src),
                };
                let tc = Instant::now();
                let r = profile::clone_profile(&src, &prof, &b.browser_dir())?;
                check_interrupt()?;
                meta.insert("profile".into(), json!(format!("{}/{}", src.display(), prof)));
                meta.insert("profile_dir".into(), json!(prof));
                meta.insert(
                    "clone".into(),
                    json!({"files": r.files, "cookies": r.cookies, "cookies_dropped": r.cookies_dropped,
                           "key": r.key_source, "ms": tc.elapsed().as_millis()}),
                );
            }
        }
        write_meta(&b, &meta)?;

        // The desktop runs as a transient service in the box's own slice.
        let exe = std::env::current_exe()?;
        let out = Command::new("systemd-run")
            .args(["--user", "--quiet", "--collect"])
            .arg(format!("--unit={}", b.unit()))
            .arg(format!("--slice={}", b.slice()))
            .arg("--description=agent-box desktop")
            .arg(format!("--property=RuntimeMaxSec={}", a.ttl))
            // When the desktop ends for any reason (down, TTL, crash), stop
            // the box's apps and delete its files, cookie copies included.
            .arg(format!("--property=ExecStopPost={} __reap {}", exe.display(), b.name))
            .args(self_env().iter().map(|kv| format!("--setenv={kv}")))
            .args(["env", "-i"])
            .args(self_env())
            .arg(&exe)
            .args(["__stack", &b.name, "--size", &a.size])
            .output()?;
        if !out.status.success() {
            bail!("systemd-run failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while b.display().is_none() {
            check_interrupt()?;
            let st = unit_state(&b.unit());
            if is_stopped(st.as_deref()) || Instant::now() > deadline {
                bail!("desktop did not start ({}); journalctl --user -u {}", st.as_deref().unwrap_or("unknown"), b.unit());
            }
            sleep(Duration::from_millis(10));
        }
        let out = systemctl(&[
            "set-property",
            "--runtime",
            &b.slice(),
            &format!("MemoryHigh={}", a.mem),
            "CPUWeight=50",
            "IOWeight=50",
        ])?;
        if !out.status.success() {
            bail!("could not set --mem {}: {}", a.mem, String::from_utf8_lossy(&out.stderr).trim());
        }
        let display = ready_display(&b)?;
        let mut summary = json!({
            "name": b.name,
            "display": format!(":{display}"),
            "cua_socket": b.cua_socket(),
            "dir": b.dir,
            "screen": format!("{w}x{h}"),
        });
        if browser.is_some() {
            check_interrupt()?;
            start_browser(&b, &a.url)?;
            let cdp = wait_cdp(&b, Duration::from_secs(20))?;
            summary["cdp"] = json!(cdp);
            summary["browser_profile"] = meta.get("profile").cloned().unwrap_or(Value::Null);
            if let Some(c) = meta.get("clone") {
                summary["clone"] = c.clone();
            }
        }
        check_interrupt()?;
        summary["ready_ms"] = json!(t0.elapsed().as_millis());
        Ok(summary)
    })();
    match result {
        Ok(summary) => {
            use std::io::Write;
            drop(lock);
            // If the desktop ended while `up` held the lock, its cleanup left
            // the files to us. A box nobody got the name of must not stay up.
            let fail = if unit_state(&b.unit()).as_deref() != Some("active") {
                Some(anyhow::anyhow!("box {} stopped during startup; journalctl --user -u {}", b.name, b.unit()))
            } else if let Err(e) = check_interrupt() {
                Some(e)
            } else {
                let mut out = std::io::stdout();
                writeln!(out, "{}", serde_json::to_string_pretty(&summary)?)
                    .and_then(|()| out.flush())
                    .err()
                    .map(|e| anyhow::anyhow!("could not print the box summary: {e}"))
            };
            if let Some(e) = fail {
                // Delete only our own box: `down` and a new `up` may have
                // replaced it since the lock was dropped.
                let mut kept = None;
                if let Some(_lock) = lock_name(&b, Duration::from_secs(30))? {
                    if my_gen.is_some() && read_gen(&b) == my_gen {
                        kept = keep_logs(&b);
                        let _ = destroy_locked(&b);
                    }
                }
                return Err(match kept {
                    Some(dir) => e.context(format!("box logs kept in {}", dir.display())),
                    None => e,
                });
            }
            let _ = writeln!(
                std::io::stderr(),
                "next: agent-box call {n} list_windows   |   agent-box shot {n}   |   agent-box down {n}",
                n = b.name
            );
            Ok(())
        }
        Err(e) => {
            let kept = keep_logs(&b);
            let _ = destroy_locked(&b);
            drop(lock);
            match kept {
                Some(dir) => Err(e.context(format!("box logs kept in {}", dir.display()))),
                None => Err(e),
            }
        }
    }
}

fn write_meta(b: &Box, meta: &Map<String, Value>) -> Result<()> {
    fs::write(b.meta_file(), serde_json::to_vec_pretty(meta)?)?;
    Ok(())
}

fn read_meta(b: &Box) -> Map<String, Value> {
    fs::read(b.meta_file())
        .ok()
        .and_then(|d| serde_json::from_slice::<Value>(&d).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn default_browser() -> Option<PathBuf> {
    ["/opt/helium-browser-bin/helium", "/usr/bin/chromium"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn last_used_profile(src: &Path) -> String {
    fs::read(src.join("Local State"))
        .ok()
        .and_then(|d| serde_json::from_slice::<Value>(&d).ok())
        .and_then(|v| v["profile"]["last_used"].as_str().map(String::from))
        .unwrap_or_else(|| "Default".into())
}

/// Start a program in the box as its own transient unit in the box's slice.
fn start_app(b: &Box, label: &str, cmd: &[String]) -> Result<String> {
    let display = ready_display(b)?;
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unit = format!("{}-{label}-{}-{}-{seq}", b.unit(), now_ms(), std::process::id());
    let out = Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect"])
        .arg(format!("--unit={unit}"))
        .arg(format!("--slice={}", b.slice()))
        // Apps start only while the desktop runs (never start it again) and
        // stop with it. `__reap` stops them if the desktop crashes.
        .arg(format!("--property=Requisite={}.service", b.unit()))
        .arg(format!("--property=PartOf={}.service", b.unit()))
        .arg(format!("--property=After={}.service", b.unit()))
        .arg(format!("--working-directory={}", b.home().display()))
        .args(["env", "-i"])
        .args(b.app_env(display).into_iter().map(|(k, v)| format!("{k}={v}")))
        .args(cmd)
        .output()?;
    if !out.status.success() {
        bail!("could not start {:?}: {}", cmd, String::from_utf8_lossy(&out.stderr));
    }
    Ok(unit)
}

fn start_browser(b: &Box, url: &str) -> Result<()> {
    let meta = read_meta(b);
    let browser = meta
        .get("browser")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(default_browser)
        .context("no browser for this box")?;
    let mut cmd = vec![
        browser.display().to_string(),
        format!("--user-data-dir={}", b.browser_dir().display()),
        "--password-store=basic".into(),
        "--ozone-platform=x11".into(),
        "--force-renderer-accessibility".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        "--remote-debugging-port=0".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--hide-crash-restore-bubble".into(),
        "--start-maximized".into(),
        "--disable-gpu".into(),
    ];
    if let Some(p) = meta.get("profile_dir").and_then(Value::as_str) {
        cmd.push(format!("--profile-directory={p}"));
    }
    cmd.push(url.to_string());
    start_app(b, "browser", &cmd)?;
    Ok(())
}

fn cdp_port(b: &Box) -> Option<u16> {
    fs::read_to_string(b.browser_dir().join("DevToolsActivePort"))
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn cdp_url(b: &Box) -> Result<String> {
    let port = cdp_port(b).context("box browser is not running; start it with `agent-box open NAME URL`")?;
    Ok(format!("http://127.0.0.1:{port}"))
}

fn wait_cdp(b: &Box, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        check_interrupt()?;
        if let Some(port) = cdp_port(b) {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(format!("http://127.0.0.1:{port}"));
            }
        }
        if Instant::now() > deadline {
            bail!("browser did not open its DevTools port; logs: journalctl --user -u '{}-browser-*'", b.unit());
        }
        sleep(Duration::from_millis(25));
    }
}

/// Stop every process in the box, check that they stopped, then delete its
/// files. Files stay when the stop cannot be confirmed.
fn destroy(b: &Box) -> Result<()> {
    let _lock = lock_name(b, Duration::from_secs(30))?
        .with_context(|| format!("box {} is busy (another agent-box command holds it)", b.name))?;
    destroy_locked(b)
}

/// `destroy` for a caller that already holds the name lock.
fn destroy_locked(b: &Box) -> Result<()> {
    let out = systemctl(&["stop", &b.slice()])?;
    let st = unit_state(&b.slice());
    if !is_stopped(st.as_deref()) {
        bail!(
            "box {} did not stop ({}): {}",
            b.name,
            st.as_deref().unwrap_or("systemd gave no answer"),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let _ = systemctl(&["reset-failed", &format!("{}.service", b.unit()), &format!("{}-*", b.unit()), &b.slice()]);
    remove_files(b)
}

fn remove_files(b: &Box) -> Result<()> {
    let (state, run) = paths::roots();
    for (dir, root) in [(&b.dir, &state), (&b.run, &run)] {
        if paths::is_inside(dir, root) && dir.exists() {
            fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))?;
        }
    }
    Ok(())
}

/// Copy a box's logs to the runtime directory (tmpfs, cleared at logout)
/// before its state directory is deleted.
fn keep_logs(b: &Box) -> Option<PathBuf> {
    let from = b.dir.join("logs");
    let (_, run) = paths::roots();
    let to = run.with_file_name("agent-box-logs").join(format!("{}-{}", b.name, now_ms()));
    let mut kept = false;
    for e in fs::read_dir(&from).ok()?.flatten() {
        if e.file_type().is_ok_and(|t| t.is_file()) {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(&to).ok()?;
            kept |= fs::copy(e.path(), to.join(e.file_name())).is_ok();
        }
    }
    kept.then_some(to)
}

/// ExecStopPost of a box's desktop: stop its apps and delete its files, so
/// cookie copies do not outlive the box. If another command holds the name
/// lock, that command deletes the files: `down` does, and `up` checks the
/// desktop after it lets go. Keeps logs when the desktop did not end cleanly.
fn reap_stopped(b: &Box) -> Result<()> {
    // Never the desktop unit itself: this runs inside its stop.
    let _ = systemctl(&["stop", &format!("{}-*.service", b.unit())]);
    let r = match lock_name(b, Duration::ZERO)? {
        Some(_lock) => {
            if std::env::var("SERVICE_RESULT").is_ok_and(|r| r != "success") {
                if let Some(dir) = keep_logs(b) {
                    let why = std::env::var("SERVICE_RESULT").unwrap_or_default();
                    eprintln!("agent-box: {} ended ({why}); logs kept in {}", b.name, dir.display());
                }
            }
            remove_files(b)
        }
        None => Ok(()),
    };
    // Then end the now-empty slice. No wait: it contains this unit.
    let _ = systemctl(&["--no-block", "stop", &b.slice()]);
    r
}

/// Delete boxes whose desktop has ended (reboot, or a cleanup that did not
/// run), so their cookie copies do not linger. Skips boxes that are still
/// being created, and boxes whose state systemd does not report. Locks only
/// boxes that are already stopped, so it never blocks a live box's cleanup.
fn reap_dead() {
    for n in paths::all_names() {
        let Ok(b) = Box::new(&n) else { continue };
        if !is_stopped(unit_state(&b.unit()).as_deref()) {
            continue;
        }
        let Ok(Some(_lock)) = lock_name(&b, Duration::ZERO) else { continue };
        if b.dir.exists() && is_stopped(unit_state(&b.unit()).as_deref()) && destroy_locked(&b).is_ok() {
            eprintln!("agent-box: removed stopped box {n}");
        }
    }
}

fn down(names: Vec<String>, all: bool) -> Result<()> {
    let known = all_boxes();
    let names = if all { known.clone() } else { names };
    if names.is_empty() {
        bail!("name a box, or pass --all");
    }
    if let Some(n) = names.iter().find(|n| !known.contains(n)) {
        bail!("no box named {n}; see `agent-box ls`");
    }
    for n in names {
        destroy(&Box::new(&n)?)?;
        println!("removed {n}");
    }
    Ok(())
}

fn ls(as_json: bool) -> Result<()> {
    let mut rows = Vec::new();
    for n in all_boxes() {
        let b = Box::new(&n)?;
        let mem = systemctl(&["show", "-p", "MemoryCurrent", "--value", &b.slice()])
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok());
        let state = unit_state(&b.unit()).unwrap_or_else(|| "unknown".into());
        let live = state == "active";
        rows.push(json!({
            "name": n,
            "display": b.display().filter(|_| live).map(|d| format!(":{d}")),
            "state": state,
            "cdp": cdp_port(&b).filter(|_| live).map(|p| format!("http://127.0.0.1:{p}")),
            "mem_mb": mem.map(|m| m / 1_048_576),
            "profile": read_meta(&b).get("profile").cloned(),
        }));
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("no boxes; create one with `agent-box up`");
    } else {
        println!("{:<10} {:<9} {:<8} {:>7}  CDP", "NAME", "STATE", "DISPLAY", "MEM_MB");
        for r in rows {
            println!(
                "{:<10} {:<9} {:<8} {:>7}  {}",
                r["name"].as_str().unwrap_or(""),
                r["state"].as_str().unwrap_or(""),
                r["display"].as_str().unwrap_or("-"),
                r["mem_mb"].as_u64().map(|m| m.to_string()).unwrap_or("-".into()),
                r["cdp"].as_str().unwrap_or("-"),
            );
        }
    }
    Ok(())
}

/// Pointer and keyboard tools. In a private box, foreground delivery reaches
/// no user window, and background raw input is refused for most browsers.
const INPUT_TOOLS: &[&str] = &[
    "click", "double_click", "right_click", "drag", "mouse_drag", "scroll", "move_cursor",
    "mouse_button_down", "mouse_button_up", "type_text", "press_key", "hotkey",
];

pub fn prepare_call_args(name: &str, tool: &str, mut args: Map<String, Value>) -> Map<String, Value> {
    args.entry("session").or_insert_with(|| json!(name));
    let semantic = args.contains_key("element_token") || args.contains_key("element_index");
    if INPUT_TOOLS.contains(&tool) && !semantic {
        args.entry("delivery_mode").or_insert_with(|| json!("foreground"));
    }
    args
}

fn call(name: &str, tool: &str, raw: Option<String>, shot: Option<PathBuf>, timeout: u64) -> Result<ExitCode> {
    let b = existing(name)?;
    ready_display(&b)?;
    let raw = match raw.as_deref() {
        Some("-") => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
        Some(r) => r.to_string(),
        None => "{}".into(),
    };
    let raw = if raw.trim().is_empty() { "{}".to_string() } else { raw };
    let args = match serde_json::from_str::<Value>(&raw).context("arguments must be a JSON object")? {
        Value::Object(m) => m,
        _ => bail!("arguments must be a JSON object"),
    };
    let args = prepare_call_args(name, tool, args);
    let mut cmd = Command::new("cua-driver");
    cmd.arg("call").arg("--socket").arg(b.cua_socket()).arg(tool);
    if let Some(f) = shot {
        cmd.arg("--screenshot-out-file").arg(f);
    }
    // `cua-driver call` forks a worker, so give it its own process group and
    // stop the whole group on timeout. The arguments go on stdin, not argv,
    // where other local users could read them (typed text, say).
    // The group is off the terminal's foreground group, so pass on the
    // signals that end us; otherwise the worker could keep typing. Block
    // them until the handler knows the group, so none is lost in between.
    // (The child starts with an empty signal mask.)
    let sigs = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
    // SAFETY: plain sigset and sigprocmask calls on a local set.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        for sig in sigs {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    let spawned = cmd.stdin(Stdio::piped()).process_group(0).spawn();
    if let Ok(child) = &spawned {
        CALL_GROUP.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
        for sig in sigs {
            // SAFETY: the handler only calls async-signal-safe kill and
            // _exit. A signal the caller ignores (nohup) stays ignored.
            unsafe {
                if libc::signal(sig, end_call_group as *const () as libc::sighandler_t) == libc::SIG_IGN {
                    libc::signal(sig, libc::SIG_IGN);
                }
            }
        }
    }
    // SAFETY: undo the block above; pending signals reach the handler now.
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
    let mut child = spawned.context("start cua-driver call")?;
    let mut stdin = child.stdin.take().context("cua-driver stdin")?;
    let payload = Value::Object(args).to_string();
    std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(payload.as_bytes());
    });
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            // SAFETY: kill(2) on our child's process group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            eprintln!("agent-box: {tool} gave no answer in {timeout} s; a modal dialog may block the accessibility read. Target the dialog window, or take a `shot`.");
            return Ok(ExitCode::from(124));
        }
        sleep(Duration::from_millis(20));
    };
    Ok(if status.success() { ExitCode::SUCCESS } else { ExitCode::from(status.code().unwrap_or(1) as u8) })
}

static CALL_GROUP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn end_call_group(sig: libc::c_int) {
    let group = CALL_GROUP.load(std::sync::atomic::Ordering::SeqCst);
    // SAFETY: kill(2) and _exit(2) are async-signal-safe.
    unsafe {
        if group > 0 {
            libc::kill(-group, libc::SIGKILL);
        }
        libc::_exit(128 + sig);
    }
}

fn shot(b: &Box, file: Option<PathBuf>) -> Result<PathBuf> {
    let display = ready_display(b)?;
    let file = match file {
        Some(f) => f,
        None => {
            let dir = b.dir.join("shots");
            fs::create_dir_all(&dir)?;
            dir.join(format!("{}.png", now_ms()))
        }
    };
    let status = Command::new("import")
        .env("DISPLAY", format!(":{display}"))
        .env("XAUTHORITY", b.xauth())
        .args(["-window", "root"])
        .arg(&file)
        .status()?;
    if !status.success() {
        bail!("screenshot failed");
    }
    Ok(file)
}

fn doctor() -> Result<ExitCode> {
    let mut ok = true;
    let mut check = |what: &str, good: bool, fix: &str| {
        println!("{} {what}{}", if good { "ok  " } else { "FAIL" }, if good { String::new() } else { format!("  ({fix})") });
        ok &= good;
    };
    let on_path = |p: &str| {
        Command::new("sh").args(["-c", &format!("command -v {p}")]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
    };
    for (bin, pkg) in [
        ("Xvfb", "xorg-server-xvfb"),
        ("openbox", "openbox"),
        ("dbus-daemon", "dbus"),
        ("dbus-send", "dbus"),
        ("cua-driver", "cua-driver-bin"),
        ("systemd-run", "systemd"),
        ("import", "imagemagick"),
        ("secret-tool", "libsecret"),
    ] {
        check(bin, on_path(bin), &format!("install {pkg}"));
    }
    for f in ["/usr/lib/at-spi-bus-launcher", "/usr/lib/at-spi2-registryd"] {
        check(f, Path::new(f).is_file(), "install at-spi2-core");
    }
    check("browser", default_browser().is_some(), "install helium-browser-bin or chromium");
    let src = profile::default_source();
    check(
        &format!("main profile {}", src.display()),
        src.join("Local State").is_file(),
        "sign in to Helium once, or pass --from/--blank",
    );
    check("systemd user manager", user_manager_ok(), "needs a systemd user session");
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_args_get_session_and_foreground() {
        let a = prepare_call_args("b1", "click", Map::from_iter([("x".into(), json!(1))]));
        assert_eq!(a["session"], "b1");
        assert_eq!(a["delivery_mode"], "foreground");
    }

    #[test]
    fn call_args_keep_semantic_and_explicit_values() {
        let a = prepare_call_args(
            "b1",
            "click",
            Map::from_iter([("element_token".into(), json!("s1:2")), ("session".into(), json!("mine"))]),
        );
        assert_eq!(a["session"], "mine");
        assert!(a.get("delivery_mode").is_none());
        let a = prepare_call_args("b1", "get_window_state", Map::new());
        assert!(a.get("delivery_mode").is_none());
    }

    #[test]
    fn unknown_state_is_never_stopped() {
        assert_eq!(parse_state("inactive\n").as_deref(), Some("inactive"));
        assert_eq!(parse_state(""), None);
        assert_eq!(parse_state("Failed to connect to bus"), None);
        assert!(is_stopped(Some("failed")));
        assert!(!is_stopped(None));
        assert!(!is_stopped(Some("deactivating")));
    }

    #[test]
    fn slice_names_parse() {
        let out = "agent-box-box1.slice loaded active active Slice /agent/box/box1\nagent-box.slice loaded active active x\nagent-box-a-b.slice loaded x\n";
        assert_eq!(slice_names(out), vec!["box1".to_string()]);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1440x900").unwrap(), (1440, 900));
        assert!(parse_size("big").is_err());
    }
}
