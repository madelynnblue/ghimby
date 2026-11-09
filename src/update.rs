//! The app's version, and daily self-update for installed release builds.

use std::path::Path;

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};

use crate::app::App;
use crate::store::now;

/// The release tag CI built this from, or "dev" for local builds.
pub const VERSION: &str = match option_env!("GHIMBY_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => "dev",
};

pub const REPO_URL: &str = "https://github.com/madelynnblue/ghimby";
const LATEST: &str = "https://api.github.com/repos/madelynnblue/ghimby/releases/latest";
const CHECK_SECS: i64 = 86400;

/// The release asset built for this platform.
fn asset() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("ghimby-darwin-arm64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("ghimby-linux-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("ghimby-linux-aarch64")
    } else {
        None
    }
}

/// Run by launchd or systemd, which restart us after we exit; nothing else would.
fn supervised() -> bool {
    std::env::var_os("XPC_SERVICE_NAME").is_some_and(|v| v == "io.ghimby") || std::env::var_os("INVOCATION_ID").is_some()
}

fn parse(v: &str) -> Option<Vec<u64>> {
    v.strip_prefix('v')?.split('.').map(|p| p.parse().ok()).collect()
}

/// Whether release tag `latest` is a higher version than `current`.
pub fn newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// At most daily, installs a newer release over this binary and exits so the service manager
/// restarts into it.
pub async fn maybe_update(app: &App) -> Result<()> {
    const KEY: &str = "update_checked_at";
    let Some(asset) = asset() else { return Ok(()) };
    if VERSION == "dev" || !supervised() || app.store.setting("auto_update")?.trim() == "false" {
        return Ok(());
    }
    if now() - app.store.setting(KEY)?.parse::<i64>().unwrap_or(0) < CHECK_SECS {
        return Ok(());
    }
    // Restarting would cut off a git fetch or clone; the next sync tries again.
    if !app.checkouts.running.lock().unwrap().is_empty() {
        return Ok(());
    }
    app.store.set_setting(KEY, &now().to_string())?;
    let token = app.github.token().await?;
    let rel: serde_json::Value = app.github.get(&token, LATEST, HeaderMap::new()).await?.json().await?;
    let tag = rel["tag_name"].as_str().unwrap_or_default();
    if !newer(tag, VERSION) {
        return Ok(());
    }
    let url = rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"] == asset)
        .and_then(|a| a["url"].as_str())
        .with_context(|| format!("release {tag} has no {asset}"))?;
    let mut h = HeaderMap::new();
    h.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
    let bytes = app.github.get(&token, url, h).await?.bytes().await?;
    let exe = std::env::current_exe()?;
    let new = exe.with_file_name(".ghimby.update");
    if let Err(e) = stage(&new, &bytes, tag).await {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    // Rename, not overwrite: the running binary keeps its old inode.
    std::fs::rename(&new, &exe).with_context(|| format!("replacing {}", exe.display()))?;
    tracing::info!("updated {VERSION} -> {tag}; exiting so the service restarts");
    std::process::exit(0);
}

/// Writes the download and checks it runs and reports `tag`, so a bad download never replaces us.
async fn stage(path: &Path, bytes: &[u8], tag: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    let out = tokio::process::Command::new(path).arg("--version").output().await?;
    let v = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !v.split_whitespace().any(|w| w == tag) {
        bail!("downloaded {tag} binary didn't report its version: {}", v.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn versions() {
        assert!(newer("v0.1.3", "v0.1.2"));
        assert!(newer("v0.10.0", "v0.9.9"));
        assert!(!newer("v0.1.2", "v0.1.2"));
        assert!(!newer("v0.1.1", "v0.1.2"));
        assert!(!newer("v0.2.0", "dev"));
        assert!(!newer("garbage", "v0.1.0"));
    }
}
