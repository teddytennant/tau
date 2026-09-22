//! `tau update`: replace this binary with the latest GitHub release, after
//! checking it against the release's sha256sums.txt.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::Read;

const REPO: &str = "teddytennant/tau";

pub fn target() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        a => bail!("no release build for {a}"),
    };
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        o => bail!("no release build for {o}"),
    };
    Ok(format!("{arch}-{os}"))
}

/// `v0.10.0` is newer than `v0.9.3`.
pub fn newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['.', '-'])
            .map_while(|p| p.parse().ok())
            .collect()
    };
    parse(latest) > parse(current)
}

fn get(url: &str) -> Result<Vec<u8>> {
    let resp = crate::provider::agent()
        .get(url)
        .header("user-agent", "tau-update")
        .call()?;
    let st = resp.status().as_u16();
    if st >= 300 {
        bail!("HTTP {st} from {url}");
    }
    let mut b = vec![];
    resp.into_body()
        .into_reader()
        .take(200 << 20)
        .read_to_end(&mut b)?;
    Ok(b)
}

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

pub fn run(check_only: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let v: serde_json::Value = serde_json::from_slice(&get(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))?)?;
    let tag = v["tag_name"]
        .as_str()
        .context("no tag_name in the release")?;
    if !newer(tag, current) {
        println!("tau {current} is the latest");
        return Ok(());
    }
    if check_only {
        println!("tau {tag} is out (you have {current}). Run `tau update`.");
        return Ok(());
    }
    let t = target()?;
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let tarball = get(&format!("{base}/tau-{t}.tar.gz"))?;
    let sums = String::from_utf8(get(&format!("{base}/sha256sums.txt"))?)?;
    let want = sums
        .lines()
        .find(|l| l.ends_with(&format!(" tau-{t}.tar.gz")))
        .and_then(|l| l.split_whitespace().next())
        .context("no checksum for this platform")?;
    if sha256_hex(&tarball) != want {
        bail!("checksum mismatch, not installing");
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().context("binary has no directory")?;
    let tmp = tempdir_in(dir)?;
    let tgz = tmp.join("tau.tar.gz");
    std::fs::write(&tgz, &tarball)?;
    let st = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tgz)
        .arg("-C")
        .arg(&tmp)
        .status()?;
    if !st.success() {
        bail!("could not unpack the release");
    }
    let new = tmp.join("tau");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    // rename over the running binary is safe on unix; the old inode lives on
    std::fs::rename(&new, &exe).with_context(|| format!("replacing {}", exe.display()))?;
    let _ = std::fs::remove_dir_all(&tmp);
    println!("updated tau {current} -> {tag} at {}", exe.display());
    Ok(())
}

fn tempdir_in(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let p = dir.join(format!(".tau-update-{}", std::process::id()));
    std::fs::create_dir_all(&p).with_context(|| {
        format!(
            "{} is not writable; reinstall with the install script",
            dir.display()
        )
    })?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(newer("v0.2.0", "0.1.0"));
        assert!(newer("v0.10.0", "0.9.3"));
        assert!(!newer("v0.2.0", "0.2.0"));
        assert!(!newer("v0.1.9", "0.2.0"));
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
