//! ~/.tau/settings.json and ~/.tau/auth.json. Environment variables win over both.

use crate::log::tau_home;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Provider name, one of `providers::KNOWN` or "custom".
    pub provider: Option<String>,
    /// Default model per provider.
    pub models: BTreeMap<String, String>,
    /// Models ctrl+p cycles through, as "provider/model".
    pub favorites: Vec<String>,
    pub thinking: String,
    pub theme: String,
    pub auto_compact: bool,
    /// Compact when the context is within this many tokens of the window.
    pub compact_reserve: u64,
    /// Recent tokens kept verbatim after an automatic compaction.
    pub compact_keep: u64,
    pub tips_shown: bool,
    /// Extra directories searched for skills, like ~/.claude/skills.
    pub skill_dirs: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            provider: None,
            models: BTreeMap::new(),
            favorites: vec![],
            thinking: "auto".into(),
            theme: "dark".into(),
            auto_compact: true,
            compact_reserve: 16_384,
            compact_keep: 20_000,
            tips_shown: false,
            skill_dirs: vec![],
        }
    }
}

pub const THINKING: [&str; 6] = ["auto", "low", "medium", "high", "xhigh", "max"];

fn settings_path() -> PathBuf {
    tau_home().join("settings.json")
}

fn auth_path() -> PathBuf {
    tau_home().join("auth.json")
}

impl Settings {
    pub fn load() -> Settings {
        fs::read_to_string(settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(
            &settings_path(),
            &serde_json::to_string_pretty(self)?,
            0o644,
        )
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Cred {
    /// An API key. Empty for an account sign-in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Tokens from an account sign-in (ChatGPT, xAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthTokens>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// ChatGPT's account id, sent on every request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Where refreshes go, from discovery at sign-in (xAI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
}

pub type Auth = BTreeMap<String, Cred>;

/// An exclusive lock on ~/.tau/auth.json.lock, held while auth.json is read,
/// changed and written. Two tau processes refreshing the same grant at once
/// would spend a single-use refresh token twice, and xAI treats the second
/// use as a revoked session. Released on drop.
pub struct AuthLock(#[allow(dead_code)] fs::File);

pub fn lock_auth(wait: std::time::Duration) -> Result<AuthLock> {
    use std::os::fd::AsRawFd;
    let path = tau_home().join("auth.json.lock");
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(AuthLock(f));
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("another tau is holding {} ", path.display());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Read, change and write auth.json under the lock, so concurrent writers
/// never lose each other's entries.
pub fn update_auth<T>(f: impl FnOnce(&mut Auth) -> T) -> Result<T> {
    let _lock = lock_auth(std::time::Duration::from_secs(15))?;
    let mut a = load_auth();
    let out = f(&mut a);
    save_auth(&a)?;
    Ok(out)
}

pub fn load_auth() -> Auth {
    fs::read_to_string(auth_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_auth(a: &Auth) -> Result<()> {
    write_atomic(&auth_path(), &serde_json::to_string_pretty(a)?, 0o600)
}

/// Writes through a temp file created with `mode`, so a secret is never
/// readable by others, not even for a moment.
pub fn write_atomic(path: &Path, contents: &str, mode: u32) -> Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    // rename keeps the temp file's mode, but an old file may have been looser
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn auth_file_is_private() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        // a pre-existing world-readable file gets tightened
        fs::write(&p, "{}").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let mut a = Auth::new();
        a.insert(
            "anthropic".into(),
            Cred {
                key: "sk-test".into(),
                base_url: None,
                oauth: None,
            },
        );
        write_atomic(&p, &serde_json::to_string(&a).unwrap(), 0o600).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back: Auth = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back, a);
        // no temp file left behind
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn settings_tolerate_missing_fields() {
        let s: Settings = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(s.theme, "light");
        assert!(s.auto_compact);
        assert_eq!(s.thinking, "auto");
    }
}
