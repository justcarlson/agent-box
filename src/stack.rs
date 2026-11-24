//! The per-box desktop: Xvfb, a private D-Bus with AT-SPI, openbox and
//! optionally a cua-driver daemon. Runs as the main process of the box's
//! systemd unit, so stopping the unit's slice ends every process in the box.

use crate::paths::AgentBox;
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Candidate directories for AT-SPI helper binaries across Linux distros
/// (Arch, Fedora/RHEL, Debian/Ubuntu).
pub const ATSPI_DIRS: &[&str] = &["/usr/lib", "/usr/libexec", "/usr/lib/at-spi2-core"];

pub fn find_atspi_binary(name: &str) -> Option<PathBuf> {
    find_atspi_in(name, ATSPI_DIRS)
}

pub(crate) fn find_atspi_in(name: &str, dirs: &[&str]) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

/// A session bus that only routes messages. No activation directories, so
/// nothing (portals, gvfs, keyring) starts in the box behind our back.
const BUS_CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=@BUS@</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) -> Result<()> {
    let start = Instant::now();
    while !ok() {
        if start.elapsed() > timeout {
            bail!("timed out after {timeout:?} waiting for {what}");
        }
        sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// One MIT-MAGIC-COOKIE-1 entry with FamilyWild and no display number, so it
/// matches whichever display Xvfb picks.
pub(crate) fn write_xauth(path: &Path) -> Result<()> {
    let mut cookie = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut cookie)?;
    let mut buf = Vec::new();
    let field = |buf: &mut Vec<u8>, data: &[u8]| {
        buf.extend((data.len() as u16).to_be_bytes());
        buf.extend(data);
    };
    buf.extend(0xffffu16.to_be_bytes());
    field(&mut buf, b"");
    field(&mut buf, b"");
    field(&mut buf, b"MIT-MAGIC-COOKIE-1");
    field(&mut buf, &cookie);
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(&buf)?;
    Ok(())
}

fn log_file(b: &AgentBox, name: &str) -> Result<Stdio> {
    let dir = b.dir.join("logs");
    fs::create_dir_all(&dir)?;
    Ok(fs::File::create(dir.join(format!("{name}.log")))?.into())
}

fn spawn(b: &AgentBox, name: &str, cmd: &mut Command) -> Result<Child> {
    cmd.stdin(Stdio::null())
        .stdout(log_file(b, name)?)
        .stderr(log_file(b, &format!("{name}.err"))?)
        .spawn()
        .with_context(|| format!("start {name}"))
}

fn dbus_send(bus: &str, args: &[&str]) -> bool {
    Command::new("dbus-send")
        .arg(format!("--bus={bus}"))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// First display number boxes use. Low numbers belong to the user's
/// session: Xwayland may hold :0 through an abstract socket and an `X0_`
/// path that Xvfb's own checks miss.
const FIRST_DISPLAY: u32 = 100;

/// True if anything already claims display `n`.
fn display_taken(n: u32, unix_table: &str) -> bool {
    let sock = format!("/tmp/.X11-unix/X{n}");
    Path::new(&format!("/tmp/.X{n}-lock")).exists()
        || Path::new(&sock).exists()
        || unix_table.lines().any(|l| {
            l.split_whitespace()
                .last()
                .is_some_and(|p| p.trim_start_matches('@') == sock || p == format!("{sock}_"))
        })
}

/// Start Xvfb on the first free display at or above FIRST_DISPLAY. Xvfb
/// writes the number to fd 3 once it accepts clients; if it exits first
/// (another box won the race for that number), try the next one.
fn start_xvfb(b: &AgentBox, width: u32, height: u32) -> Result<(Child, u32)> {
    let unix_table = fs::read_to_string("/proc/net/unix").unwrap_or_default();
    for n in FIRST_DISPLAY..FIRST_DISPLAY + 400 {
        if display_taken(n, &unix_table) {
            continue;
        }
        let mut fds = [0; 2];
        // SAFETY: plain pipe2(2); both ends are owned right after.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            bail!("pipe failed");
        }
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let wr_raw = fds[1];
        let mut xvfb = Command::new("Xvfb");
        xvfb.arg(format!(":{n}"))
            .args(["-displayfd", "3", "-nolisten", "tcp"])
            .arg("-auth")
            .arg(b.xauth())
            .args([
                "-screen",
                "0",
                &format!("{width}x{height}x24"),
                "-dpi",
                "96",
            ]);
        // SAFETY: only async-signal-safe dup2 between fork and exec.
        unsafe {
            xvfb.pre_exec(move || {
                if libc::dup2(wr_raw, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = spawn(b, "xvfb", &mut xvfb)?;
        drop(wr);
        let mut line = String::new();
        BufReader::new(fs::File::from(rd)).read_line(&mut line)?;
        if line.trim() == n.to_string() {
            return Ok((child, n));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    bail!(
        "no free X display between :{FIRST_DISPLAY} and :{}",
        FIRST_DISPLAY + 400
    )
}

pub fn run(b: &AgentBox, width: u32, height: u32, with_cua: bool) -> Result<()> {
    fs::create_dir_all(&b.run)?;
    fs::create_dir_all(b.home())?;
    let _ = fs::remove_file(b.ready_file());
    write_xauth(&b.xauth())?;
    let mut children: Vec<(&str, Child)> = Vec::new();

    let (xvfb, display) = start_xvfb(b, width, height)?;
    children.push(("xvfb", xvfb));

    let env = b.app_env(display);
    let cmd = |prog: &OsStr| {
        let mut c = Command::new(prog);
        c.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
        c
    };

    // Private session bus.
    let bus_conf = b.run.join("bus.conf");
    fs::write(
        &bus_conf,
        BUS_CONFIG.replace("@BUS@", &b.bus().display().to_string()),
    )?;
    let _ = fs::remove_file(b.bus());
    children.push((
        "dbus",
        spawn(
            b,
            "dbus",
            cmd(OsStr::new("dbus-daemon"))
                .arg("--nofork")
                .arg(format!("--config-file={}", bus_conf.display())),
        )?,
    ));
    wait_for("D-Bus", Duration::from_secs(5), || b.bus().exists())?;
    let bus = format!("unix:path={}", b.bus().display());

    // Accessibility bus, switched on, plus its registry.
    let atspi_launcher = find_atspi_binary("at-spi-bus-launcher").context(
        "at-spi-bus-launcher not found in /usr/lib, /usr/libexec, or /usr/lib/at-spi2-core",
    )?;
    let atspi_registryd = find_atspi_binary("at-spi2-registryd").context(
        "at-spi2-registryd not found in /usr/lib, /usr/libexec, or /usr/lib/at-spi2-core",
    )?;
    children.push((
        "atspi",
        spawn(
            b,
            "atspi",
            cmd(atspi_launcher.as_os_str()).arg("--launch-immediately"),
        )?,
    ));
    wait_for("AT-SPI bus", Duration::from_secs(5), || {
        dbus_send(
            &bus,
            &[
                "--print-reply",
                "--dest=org.a11y.Bus",
                "/org/a11y/bus",
                "org.a11y.Bus.GetAddress",
            ],
        )
    })?;
    dbus_send(
        &bus,
        &[
            "--print-reply",
            "--dest=org.a11y.Bus",
            "/org/a11y/bus",
            "org.freedesktop.DBus.Properties.Set",
            "string:org.a11y.Status",
            "string:IsEnabled",
            "variant:boolean:true",
        ],
    );
    children.push((
        "registryd",
        spawn(b, "registryd", &mut cmd(atspi_registryd.as_os_str()))?,
    ));
    children.push((
        "openbox",
        spawn(b, "openbox", &mut cmd(OsStr::new("openbox")))?,
    ));

    if with_cua {
        // cua-driver reads its settings (telemetry choice, history) from the real HOME.
        let _ = fs::remove_file(b.cua_socket());
        let mut cua = cmd(OsStr::new("cua-driver"));
        cua.env("HOME", crate::paths::home())
            .arg("serve")
            .arg("--socket")
            .arg(b.cua_socket())
            .arg("--pid-file")
            .arg(b.run.join("cua.pid"))
            .arg("--no-overlay");
        children.push(("cua-driver", spawn(b, "cua-driver", &mut cua)?));
        wait_for("cua-driver socket", Duration::from_secs(10), || {
            b.cua_socket().exists()
        })?;
    }

    fs::write(b.ready_file(), format!("{display}\n"))?;

    // Supervise: if any part of the desktop dies, the box is broken; exit so
    // systemd marks the unit failed and `agent-box ls` shows it.
    loop {
        for (name, child) in children.iter_mut() {
            if let Some(status) = child.try_wait()? {
                let _ = fs::remove_file(b.ready_file());
                bail!("{name} exited ({status}); box {} is down", b.name);
            }
        }
        sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_abstract_and_suffixed_x_sockets() {
        let table = "Num RefCount Protocol Flags Type St Inode Path
0000: 00000002 00000000 00010000 0001 01 23665 @/tmp/.X11-unix/X101
0000: 00000002 00000000 00010000 0001 01 23664 /tmp/.X11-unix/X102_
";
        assert!(display_taken(101, table));
        assert!(display_taken(102, table));
        assert!(!display_taken(103, table));
    }

    #[test]
    fn finds_atspi_across_candidate_dirs() {
        let root = std::env::temp_dir().join(format!("agent-box-atspi-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d1 = root.join("lib");
        let d2 = root.join("libexec");
        let d3 = root.join("lib/at-spi2-core");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        fs::create_dir_all(&d3).unwrap();
        fs::write(d3.join("at-spi-bus-launcher"), "").unwrap();
        fs::write(d2.join("at-spi2-registryd"), "").unwrap();

        let s1 = d1.to_string_lossy().into_owned();
        let s2 = d2.to_string_lossy().into_owned();
        let s3 = d3.to_string_lossy().into_owned();
        let dirs = [s1.as_str(), s2.as_str(), s3.as_str()];

        assert_eq!(
            find_atspi_in("at-spi-bus-launcher", &dirs),
            Some(d3.join("at-spi-bus-launcher"))
        );
        assert_eq!(
            find_atspi_in("at-spi2-registryd", &dirs),
            Some(d2.join("at-spi2-registryd"))
        );
        assert_eq!(find_atspi_in("missing", &dirs), None);

        fs::remove_dir_all(&root).unwrap();
    }
}
