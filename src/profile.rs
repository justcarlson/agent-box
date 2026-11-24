//! Clone a Chromium-family profile so a sandbox starts signed in like it.
//!
//! The clone is copy-on-write (btrfs/XFS reflink) and skips caches, history,
//! extensions (by default) and saved passwords. Cookies are re-encrypted from
//! the keyring key (`v11`) to Chromium's fixed "basic" key (`v10`), so the
//! sandbox browser needs no keyring and the keyring password never leaves this
//! process.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::{Command, Stdio};

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const IV: [u8; 16] = [b' '; 16];
const BASIC_PASSWORD: &[u8] = b"peanuts";

/// Profile entries that are caches, browsing history, open sessions, or
/// secrets we do not hand to agents. Matched against the path relative to the
/// profile directory.
const SKIP: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "ShaderCache",
    "Service Worker/CacheStorage",
    "blob_storage",
    "Sessions",
    "Sessions_Encrypted",
    "Current Session",
    "Current Tabs",
    "Last Session",
    "Last Tabs",
    "History",
    "History-journal",
    "Visited Links",
    "Top Sites",
    "Top Sites-journal",
    "Favicons",
    "Favicons-journal",
    "Shortcuts",
    "Shortcuts-journal",
    "Network Action Predictor",
    "Network Action Predictor-journal",
    "Login Data",
    "Login Data-journal",
    "Login Data For Account",
    "Login Data For Account-journal",
    "Web Data",
    "Web Data-journal",
    "LOCK",
    "DevToolsActivePort",
];

/// Browser extension directories skipped by default unless `--with-extensions`
/// is passed on `agent-box up`.
const SKIP_EXTENSIONS: &[&str] = &[
    "Extensions",
    "Local Extension Settings",
    "Sync Extension Settings",
    "Extension State",
    "Extension Rules",
    "Extension Scripts",
    "Extension Cookies",
    "Extension Cookies-journal",
    "Managed Extension Settings",
    "Storage/ext",
];

pub struct CloneOptions<'a> {
    pub keyring_apps: &'a [&'a str],
    pub with_extensions: bool,
}

pub struct CloneReport {
    pub files: usize,
    pub cookies: usize,
    pub cookies_dropped: usize,
    pub key_source: String,
}

pub fn key_from_password(password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", 1, &mut key);
    key
}

pub fn decrypt(key: &[u8; 16], ciphertext: &[u8]) -> Option<Vec<u8>> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return None;
    }
    Aes128CbcDec::new(key.into(), &IV.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .ok()
}

pub fn encrypt(key: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    Aes128CbcEnc::new(key.into(), &IV.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// Cookie DB version 24 and later prefix the value with SHA-256(host_key).
fn plaintext_ok(plain: &[u8], host: &str, hashed: bool) -> bool {
    if !hashed {
        return true;
    }
    plain.len() >= 32 && plain[..32] == Sha256::digest(host.as_bytes())[..]
}

fn should_skip(rel: &Path, with_extensions: bool) -> bool {
    if SKIP.iter().any(|s| Path::new(s) == rel) {
        return true;
    }
    if with_extensions {
        return false;
    }
    if SKIP_EXTENSIONS.iter().any(|s| Path::new(s) == rel) {
        return true;
    }
    rel.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("chrome-extension_"))
}

/// Clone `src_root/<profile>` (plus a trimmed `Local State`) into `dst_root`.
pub fn clone_profile(
    src_root: &Path,
    profile: &str,
    dst_root: &Path,
    opts: &CloneOptions<'_>,
) -> Result<CloneReport> {
    let mut parts = Path::new(profile).components();
    if !matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) {
        bail!("profile must be one directory name inside the browser data directory: {profile:?}");
    }
    let src_profile = src_root.join(profile);
    if !src_profile.join("Preferences").is_file() {
        bail!("no browser profile at {}", src_profile.display());
    }
    mkdir_private(dst_root)?;
    let dst_profile = dst_root.join(profile);
    let mut files = 0;
    copy_tree(
        &src_profile,
        &dst_profile,
        Path::new(""),
        opts.with_extensions,
        &mut files,
    )?;
    write_local_state(src_root, profile, dst_root)?;
    fix_preferences(&dst_profile)?;

    let mut report = CloneReport {
        files,
        cookies: 0,
        cookies_dropped: 0,
        key_source: "none".into(),
    };
    for rel in ["Cookies", "Network/Cookies"] {
        let db = dst_profile.join(rel);
        if db.is_file() {
            ensure_consistent_db(&src_profile.join(rel), &db)?;
            reencrypt_cookies(
                &db,
                opts.keyring_apps,
                is_kde_session_env(),
                &mut report,
                keyring_password,
            )?;
        }
    }
    Ok(report)
}

/// The live browser may be mid-write while we copy. Its rollback journal is
/// copied with the database, so opening the copy rolls back any partial
/// transaction. If the copy still fails an integrity check, copy again.
fn ensure_consistent_db(src: &Path, dst: &Path) -> Result<()> {
    let journal = |p: &Path| {
        p.with_file_name(format!(
            "{}-journal",
            p.file_name().unwrap().to_string_lossy()
        ))
    };
    for attempt in 0..5 {
        if attempt > 0 {
            for (from, to) in [
                (src.to_path_buf(), dst.to_path_buf()),
                (journal(src), journal(dst)),
            ] {
                let _ = fs::remove_file(&to);
                if from.is_file() {
                    clone_file(&from, &to)?;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let ok = rusqlite::Connection::open(dst)
            .and_then(|c| c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)))
            .is_ok_and(|r| r == "ok");
        if ok {
            return Ok(());
        }
    }
    bail!("could not get a consistent copy of {}", src.display())
}

fn mkdir_private(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))
}

fn copy_tree(
    src: &Path,
    dst: &Path,
    rel: &Path,
    with_extensions: bool,
    files: &mut usize,
) -> Result<()> {
    mkdir_private(dst)?;
    for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let rel_child = rel.join(&name);
        if should_skip(&rel_child, with_extensions) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let kind = entry.file_type()?;
        // Symlinks are skipped: a link copied as-is would point the box back
        // into the main profile, and the box would write through it.
        if kind.is_dir() {
            copy_tree(&from, &to, &rel_child, with_extensions, files)?;
        } else if kind.is_file() {
            clone_file(&from, &to).with_context(|| format!("copy {}", from.display()))?;
            *files += 1;
        }
    }
    Ok(())
}

/// Reflink when the filesystem supports it; otherwise a normal copy.
fn clone_file(from: &Path, to: &Path) -> Result<()> {
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let src = fs::File::open(from)?;
    let dst = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    // SAFETY: both descriptors are open for the duration of the call.
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if rc != 0 {
        drop(dst);
        fs::remove_file(to)?;
        fs::copy(from, to)?;
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Keep only the cloned profile, and drop the keyring and portal key
/// providers so the sandbox uses `--password-store=basic` cleanly.
pub fn trim_local_state(mut state: Value, profile: &str) -> Value {
    if let Some(obj) = state.as_object_mut() {
        obj.remove("os_crypt");
        let prof = obj.entry("profile").or_insert_with(|| json!({}));
        if let Some(p) = prof.as_object_mut() {
            if let Some(cache) = p.get_mut("info_cache").and_then(Value::as_object_mut) {
                cache.retain(|k, _| k == profile);
            }
            p.insert("last_used".into(), json!(profile));
            p.insert("last_active_profiles".into(), json!([profile]));
            p.insert("show_picker_on_startup".into(), json!(false));
            p.insert("picker_shown".into(), json!(true));
        }
    }
    state
}

fn write_local_state(src_root: &Path, profile: &str, dst_root: &Path) -> Result<()> {
    let src = src_root.join("Local State");
    let state = if src.is_file() {
        read_json(&src)?
    } else {
        json!({})
    };
    let state = trim_local_state(state, profile);
    fs::write(dst_root.join("Local State"), serde_json::to_vec(&state)?)?;
    Ok(())
}

/// The source browser is usually running, so its Preferences say it did not
/// exit. Mark a clean exit so the clone opens without a restore prompt.
fn fix_preferences(dst_profile: &Path) -> Result<()> {
    let path = dst_profile.join("Preferences");
    let mut prefs = read_json(&path)?;
    if let Some(obj) = prefs.as_object_mut() {
        let p = obj.entry("profile").or_insert_with(|| json!({}));
        if let Some(p) = p.as_object_mut() {
            p.insert("exit_type".into(), json!("Normal"));
            p.insert("exited_cleanly".into(), json!(true));
        }
    }
    fs::write(&path, serde_json::to_vec(&prefs)?)?;
    Ok(())
}

fn is_kde_session(xdg_current_desktop: Option<&str>, kde_full_session: Option<&str>) -> bool {
    xdg_current_desktop.is_some_and(|v| v.to_ascii_lowercase().contains("kde"))
        || kde_full_session.is_some_and(|v| !v.is_empty())
}

fn is_kde_session_env() -> bool {
    is_kde_session(
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        std::env::var("KDE_FULL_SESSION").ok().as_deref(),
    )
}

fn cookie_decrypt_error(keyring_apps: &[&str], kde_session: bool) -> String {
    let mut msg = format!(
        "could not decrypt the profile's cookies with any keyring entry ({}); is the keyring unlocked?",
        keyring_apps.join(", ")
    );
    if kde_session {
        msg.push_str(
            "\nnote: KDE KWallet is not yet supported in v0.1; use GNOME Keyring / Secret Service or pass --blank",
        );
    }
    msg
}

/// Read a keyring secret. `secret-tool search` without `--unlock` returns
/// only unlocked items, so it never puts an unlock prompt on the user's
/// screen. Give up after a few seconds anyway.
fn keyring_password(app: &str) -> Option<Vec<u8>> {
    let mut child = Command::new("timeout")
        .args(["5", "secret-tool", "search", "application", app])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = Vec::new();
    child.stdout.take()?.read_to_end(&mut out).ok()?;
    let ok = child.wait().ok()?.success();
    let pw = secret_from_search(&out);
    out.fill(0);
    pw.filter(|_| ok)
}

/// The `secret = ...` value from `secret-tool search` output.
fn secret_from_search(out: &[u8]) -> Option<Vec<u8>> {
    out.split(|&c| c == b'\n')
        .find_map(|l| l.strip_prefix(b"secret = "))
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
}

/// Newest cookie database schema this code understands. Newer schemas may
/// change the plaintext layout, so refuse them rather than corrupt cookies.
const MAX_COOKIE_DB_VERSION: i64 = 24;

/// Rewrite `v11` (keyring) cookies as `v10` (basic store). `password_for`
/// returns the keyring password for an application name, if any.
fn reencrypt_cookies(
    db: &Path,
    keyring_apps: &[&str],
    kde_session: bool,
    report: &mut CloneReport,
    password_for: impl Fn(&str) -> Option<Vec<u8>>,
) -> Result<()> {
    let mut conn = rusqlite::Connection::open(db)?;
    let version: i64 = conn
        .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if !(1..=MAX_COOKIE_DB_VERSION).contains(&version) {
        bail!("{}: cookie database version {version} is not supported (max {MAX_COOKIE_DB_VERSION}); update agent-box", db.display());
    }
    let hashed = version >= 24;

    // Only v10 (basic key) and v11 (keyring key) are understood. Anything
    // else (v12 from the secret portal, say) would vanish silently in the box.
    let unknown: Vec<(String, i64)> = conn
        .prepare(
            "SELECT CAST(substr(encrypted_value,1,3) AS TEXT), count(*) FROM cookies
             WHERE length(encrypted_value) > 0
               AND substr(encrypted_value,1,3) NOT IN (CAST('v10' AS BLOB), CAST('v11' AS BLOB))
             GROUP BY 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if !unknown.is_empty() {
        bail!(
            "{}: cookies use unsupported encryption {unknown:?}; only v10 and v11 are supported",
            db.display()
        );
    }

    let rows: Vec<(i64, String, Vec<u8>)> = conn
        .prepare("SELECT rowid, host_key, encrypted_value FROM cookies WHERE substr(encrypted_value,1,3)=CAST('v11' AS BLOB)")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(());
    }

    // Pick the keyring entry that actually decrypts this profile's cookies.
    let mut key11 = None;
    for app in keyring_apps {
        if let Some(pw) = password_for(app) {
            let key = key_from_password(&pw);
            let hits = rows
                .iter()
                .take(20)
                .filter(|(_, host, ev)| {
                    decrypt(&key, &ev[3..]).is_some_and(|p| plaintext_ok(&p, host, hashed))
                })
                .count();
            if hits * 2 > rows.len().min(20) {
                key11 = Some(key);
                report.key_source = format!("keyring application={app}");
                break;
            }
        }
    }
    let Some(key11) = key11 else {
        bail!("{}", cookie_decrypt_error(keyring_apps, kde_session));
    };
    let key10 = key_from_password(BASIC_PASSWORD);

    let tx = conn.transaction()?;
    {
        let mut update = tx.prepare("UPDATE cookies SET encrypted_value=?1 WHERE rowid=?2")?;
        let mut delete = tx.prepare("DELETE FROM cookies WHERE rowid=?1")?;
        for (rowid, host, ev) in rows {
            match decrypt(&key11, &ev[3..]).filter(|p| plaintext_ok(p, &host, hashed)) {
                Some(plain) => {
                    let mut out = b"v10".to_vec();
                    out.extend(encrypt(&key10, &plain));
                    update.execute(rusqlite::params![out, rowid])?;
                    report.cookies += 1;
                }
                None => {
                    delete.execute([rowid])?;
                    report.cookies_dropped += 1;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_output_secret() {
        let out = b"[/1]\nlabel = Chromium Safe Storage\nsecret = abc123==\nattribute.application = chromium\n";
        assert_eq!(secret_from_search(out).as_deref(), Some(&b"abc123=="[..]));
        assert_eq!(secret_from_search(b""), None);
    }

    #[test]
    fn profile_name_is_one_component() {
        let tmp = std::env::temp_dir();
        let opts = CloneOptions {
            keyring_apps: &["chromium"],
            with_extensions: false,
        };
        for bad in ["../Default", "a/b", "/etc", "", "."] {
            assert!(
                clone_profile(&tmp, bad, &tmp.join("x"), &opts).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn copy_tree_skips_symlinks_and_extensions_by_default() {
        let root = std::env::temp_dir().join(format!("agent-box-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (src, dst_no_ext, dst_with_ext) = (
            root.join("src"),
            root.join("dst_no_ext"),
            root.join("dst_with_ext"),
        );
        fs::create_dir_all(src.join("Cache")).unwrap();
        fs::create_dir_all(src.join("Extensions/abc")).unwrap();
        fs::create_dir_all(src.join("Local Extension Settings/abc")).unwrap();
        fs::create_dir_all(src.join("Storage/ext/abc")).unwrap();
        fs::create_dir_all(src.join("Storage/keep")).unwrap();
        fs::write(src.join("Preferences"), "{}").unwrap();
        fs::write(src.join("Cache/x"), "cached").unwrap();
        fs::write(src.join("Extensions/abc/manifest.json"), "{}").unwrap();
        fs::write(src.join("Local Extension Settings/abc/data"), "secret").unwrap();
        fs::write(src.join("Storage/ext/abc/data"), "ext").unwrap();
        fs::write(src.join("Storage/keep/data"), "ok").unwrap();
        std::os::unix::fs::symlink(&root, src.join("escape")).unwrap();

        let mut files = 0;
        copy_tree(&src, &dst_no_ext, Path::new(""), false, &mut files).unwrap();
        assert_eq!(files, 2);
        assert!(dst_no_ext.join("Preferences").is_file());
        assert!(dst_no_ext.join("Storage/keep/data").is_file());
        assert!(!dst_no_ext.join("Cache").exists());
        assert!(!dst_no_ext.join("Extensions").exists());
        assert!(!dst_no_ext.join("Local Extension Settings").exists());
        assert!(!dst_no_ext.join("Storage/ext").exists());
        assert!(fs::symlink_metadata(dst_no_ext.join("escape")).is_err());

        let mut files_with_ext = 0;
        copy_tree(
            &src,
            &dst_with_ext,
            Path::new(""),
            true,
            &mut files_with_ext,
        )
        .unwrap();
        assert_eq!(files_with_ext, 5);
        assert!(dst_with_ext.join("Extensions/abc/manifest.json").is_file());
        assert!(dst_with_ext
            .join("Local Extension Settings/abc/data")
            .is_file());
        assert!(dst_with_ext.join("Storage/ext/abc/data").is_file());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn roundtrip_v10() {
        let key = key_from_password(BASIC_PASSWORD);
        let host = ".example.com";
        let mut plain = Sha256::digest(host.as_bytes()).to_vec();
        plain.extend(b"session=abc");
        let ct = encrypt(&key, &plain);
        let back = decrypt(&key, &ct).unwrap();
        assert_eq!(back, plain);
        assert!(plaintext_ok(&back, host, true));
        assert!(!plaintext_ok(&back, ".other.com", true));
    }

    #[test]
    fn known_peanuts_key() {
        // Chromium's documented Linux basic key: PBKDF2-SHA1("peanuts", "saltysalt", 1, 16).
        let key = key_from_password(b"peanuts");
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "fd621fe5a2b402539dfa147ca9272778");
    }

    #[test]
    fn kde_session_detection_and_error_note() {
        assert!(is_kde_session(Some("KDE"), None));
        assert!(is_kde_session(Some("plasma:kde"), None));
        assert!(is_kde_session(None, Some("true")));
        assert!(!is_kde_session(Some("Hyprland"), None));
        assert!(!is_kde_session(None, Some("")));

        let with_kde = cookie_decrypt_error(&["chrome", "chromium"], true);
        assert!(with_kde.contains("KDE KWallet"), "{with_kde}");
        assert!(with_kde.contains("--blank"), "{with_kde}");
        assert!(with_kde.contains("chrome, chromium"), "{with_kde}");

        let without_kde = cookie_decrypt_error(&["chrome", "chromium"], false);
        assert!(!without_kde.contains("KWallet"), "{without_kde}");
    }

    #[test]
    fn reencrypts_v11_cookie_db() {
        let dir = std::env::temp_dir().join(format!("agent-box-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");
        let _ = fs::remove_file(&db);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT, value TEXT); INSERT INTO meta VALUES('version','24');
             CREATE TABLE cookies(host_key TEXT, encrypted_value BLOB);",
        )
        .unwrap();
        let k11 = key_from_password(b"keyring-secret");
        let host = ".example.com";
        let mut plain = Sha256::digest(host.as_bytes()).to_vec();
        plain.extend(b"session=abc");
        let mut good = b"v11".to_vec();
        good.extend(encrypt(&k11, &plain));
        let mut bad = b"v11".to_vec();
        bad.extend(encrypt(&key_from_password(b"other"), &plain));
        for ev in [&good, &good, &bad] {
            conn.execute(
                "INSERT INTO cookies VALUES(?1, ?2)",
                rusqlite::params![host, ev],
            )
            .unwrap();
        }
        drop(conn);

        let apps = ["helium", "chromium"];
        let mut report = CloneReport {
            files: 0,
            cookies: 0,
            cookies_dropped: 0,
            key_source: "none".into(),
        };
        reencrypt_cookies(&db, &apps, false, &mut report, |app| {
            (app == "chromium").then(|| b"keyring-secret".to_vec())
        })
        .unwrap();
        assert_eq!((report.cookies, report.cookies_dropped), (2, 1));
        assert_eq!(report.key_source, "keyring application=chromium");

        let conn = rusqlite::Connection::open(&db).unwrap();
        let evs: Vec<Vec<u8>> = conn
            .prepare("SELECT encrypted_value FROM cookies")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(evs.len(), 2);
        let k10 = key_from_password(BASIC_PASSWORD);
        for ev in evs {
            assert_eq!(&ev[..3], b"v10");
            assert_eq!(decrypt(&k10, &ev[3..]).unwrap(), plain);
        }

        // No key that works: refuse rather than silently drop every cookie; include KDE note when KDE.
        let db2 = dir.join("Cookies2");
        let _ = fs::remove_file(&db2);
        let conn = rusqlite::Connection::open(&db2).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT, value TEXT); INSERT INTO meta VALUES('version','24');
             CREATE TABLE cookies(host_key TEXT, encrypted_value BLOB);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cookies VALUES(?1, ?2)",
            rusqlite::params![host, &good],
        )
        .unwrap();
        drop(conn);
        let mut report = CloneReport {
            files: 0,
            cookies: 0,
            cookies_dropped: 0,
            key_source: "none".into(),
        };
        let err = reencrypt_cookies(&db2, &["chrome", "chromium"], true, &mut report, |_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("KDE KWallet"), "{err}");

        // A prefix we do not understand fails loudly.
        let db3 = dir.join("Cookies3");
        let _ = fs::remove_file(&db3);
        let conn = rusqlite::Connection::open(&db3).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT, value TEXT); INSERT INTO meta VALUES('version','24');
             CREATE TABLE cookies(host_key TEXT, encrypted_value BLOB);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cookies VALUES(?1, ?2)",
            rusqlite::params![host, &good],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cookies VALUES(?1, ?2)",
            rusqlite::params![host, b"v12xxxxxxxxxxxxxxxx".to_vec()],
        )
        .unwrap();
        drop(conn);
        let err = reencrypt_cookies(&db3, &apps, false, &mut report, |_| {
            Some(b"keyring-secret".to_vec())
        })
        .unwrap_err();
        assert!(err.to_string().contains("v12"), "{err}");

        // So does a schema newer than we know.
        let conn = rusqlite::Connection::open(&db3).unwrap();
        conn.execute_batch("DELETE FROM cookies WHERE substr(encrypted_value,1,3)=CAST('v12' AS BLOB); UPDATE meta SET value='25';").unwrap();
        drop(conn);
        let err = reencrypt_cookies(&db3, &apps, false, &mut report, |_| {
            Some(b"keyring-secret".to_vec())
        })
        .unwrap_err();
        assert!(err.to_string().contains("version 25"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wrong_key_fails() {
        let a = key_from_password(b"a");
        let b = key_from_password(b"b");
        let ct = encrypt(&a, b"0123456789abcdef0123456789abcdefxyz");
        let out = decrypt(&b, &ct);
        assert!(out.is_none() || !plaintext_ok(&out.unwrap(), "x", true));
    }

    #[test]
    fn trims_local_state() {
        let state = json!({
            "os_crypt": {"portal": {"prev_desktop": "Hyprland"}},
            "profile": {"info_cache": {"Default": {"name": "Work"}, "Profile 2": {"name": "Other"}},
                        "last_used": "Profile 2"}
        });
        let out = trim_local_state(state, "Default");
        assert!(out.get("os_crypt").is_none());
        let cache = out["profile"]["info_cache"].as_object().unwrap();
        assert_eq!(cache.keys().collect::<Vec<_>>(), vec!["Default"]);
        assert_eq!(out["profile"]["last_used"], "Default");
    }

    #[test]
    fn skip_list_matches_relative_paths() {
        let rel = Path::new("Service Worker").join("CacheStorage");
        assert!(should_skip(&rel, false));
        let keep = Path::new("Service Worker").join("ScriptCache");
        assert!(!should_skip(&keep, false));
        let ext = Path::new("Storage").join("ext");
        assert!(should_skip(&ext, false));
        assert!(!should_skip(&ext, true));
        let idb_ext = Path::new("IndexedDB").join("chrome-extension_abc_0.indexeddb.leveldb");
        assert!(should_skip(&idb_ext, false));
        assert!(!should_skip(&idb_ext, true));
        let ext_cookies = Path::new("Extension Cookies");
        assert!(should_skip(ext_cookies, false));
        assert!(!should_skip(ext_cookies, true));
    }
}
