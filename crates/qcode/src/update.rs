//! Version checks and self-update against GitHub Releases.
//!
//! qcode never replaces itself silently. `qc update` is an explicit command;
//! the startup check only prints a notice built from a cached answer, so a
//! normal run costs one file read and no network round trip.
//!
//! Trust model: the update binary is fetched over TLS from the release that
//! the checksums file came with, and the SHA-256 is verified before anything
//! touches the installed binary. That catches corruption and truncated
//! downloads, not a compromised release: the real boundary is the platform
//! signature (Authenticode / Developer ID), which this build does not yet have.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use serde::{Deserialize, Serialize};

/// The version this binary was built as; compared against the newest release.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const API_BASE: &str = "https://api.github.com";
const TIMEOUT: Duration = Duration::from_secs(20);
const NOTICE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("update check failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("release manifest is not understood: {0}")]
    Manifest(String),
    #[error("HTTP {status} from {url}")]
    Status { status: u16, url: String },
    #[error("{repo} has no published release yet")]
    NoRelease { repo: String },
    #[error("release {version} has no asset for this platform ({platform})")]
    NoAsset { version: String, platform: String },
    #[error("checksum mismatch for {name}: expected {expected}, got {actual}")]
    Checksum {
        name: String,
        expected: String,
        actual: String,
    },
    #[error("release {0} does not publish SHA256SUMS; refusing to install an unverified binary")]
    Unverified(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only report whether a newer release exists.
    #[arg(long)]
    pub check: bool,
    /// Install without asking for confirmation (required when not on a TTY).
    #[arg(long)]
    pub yes: bool,
    /// Install to this path instead of the running binary (packaging, tests).
    #[arg(long, value_name = "PATH", hide = true)]
    pub install_to: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset_for(&self, platform: &str) -> Option<&Asset> {
        let name = asset_name(&self.version, platform);
        self.assets.iter().find(|asset| asset.name == name)
    }

    fn checksums(&self) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == "SHA256SUMS")
    }
}

/// The `{os}-{arch}` tag used in asset names.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Raw binary asset name. Archives are for humans and package managers; the
/// updater downloads the binary itself so it needs no archive decoder.
pub fn asset_name(version: &str, platform: &str) -> String {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("qc-{version}-{platform}{suffix}")
}

pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update-check.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    latest: Option<String>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Compare release versions. A pre-release sorts before the release it leads
/// up to, so `0.2.0-rc1` does not prompt an upgrade from `0.1.0`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (latest_core, latest_pre) = split_prerelease(latest);
    let (current_core, current_pre) = split_prerelease(current);
    match compare_core(latest_core, current_core) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Less) => false,
        Some(std::cmp::Ordering::Equal) => latest_pre.is_none() && current_pre.is_some(),
        None => false,
    }
}

fn split_prerelease(version: &str) -> (&str, Option<&str>) {
    match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    }
}

fn compare_core(latest: &str, current: &str) -> Option<std::cmp::Ordering> {
    let parse = |text: &str| -> Option<Vec<u64>> {
        text.trim_start_matches('v')
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    };
    let mut left = parse(latest)?;
    let mut right = parse(current)?;
    left.resize(3, 0);
    right.resize(3, 0);
    Some(left.cmp(&right))
}

/// Pick the SHA-256 for `name` out of a `sha256sum`-style file.
pub fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        let file = file.rsplit(['/', '\\']).next().unwrap_or(file);
        (file == name && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = data.to_vec();
    let bits = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());

    let mut state = H0;
    for chunk in message.chunks(64) {
        let mut words = [0u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let bytes = [
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ];
            *word = u32::from_be_bytes(bytes);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}

fn client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(format!("qcode/{CURRENT_VERSION}"))
        .build()
}

fn release_url(config: &super::config::UpdateConfig) -> String {
    let base = config
        .base_url
        .as_deref()
        .unwrap_or(API_BASE)
        .trim_end_matches('/');
    format!("{base}/repos/{}/releases/latest", config.repo)
}

pub async fn fetch_latest(config: &super::config::UpdateConfig) -> Result<Release, UpdateError> {
    let url = release_url(config);
    let response = client(TIMEOUT)?.get(&url).send().await?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Err(UpdateError::NoRelease {
            repo: config.repo.clone(),
        });
    }
    if !status.is_success() {
        return Err(UpdateError::Status {
            status: status.as_u16(),
            url,
        });
    }
    let body: serde_json::Value = response.json().await?;
    let tag = body
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| UpdateError::Manifest("no tag_name".into()))?;
    let version = tag.trim_start_matches('v').to_string();
    let assets = body
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    Some(Asset {
                        name: asset.get("name")?.as_str()?.to_string(),
                        url: asset.get("browser_download_url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { version, assets })
}

async fn download(url: &str) -> Result<Vec<u8>, UpdateError> {
    let response = client(TIMEOUT)?.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateError::Status {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    Ok(response.bytes().await?.to_vec())
}

/// Replace `current_exe` with `bytes`, keeping the old file next to it.
///
/// The running image cannot be deleted on Windows but it can be renamed, which
/// is what makes an in-place update possible at all. On failure the backup is
/// moved back so the next launch still works. A target that does not exist yet
/// is simply written (packaging install).
pub fn install(bytes: &[u8], current_exe: &Path) -> Result<Option<PathBuf>, UpdateError> {
    let stamp = now_secs();
    let backup = PathBuf::from(format!("{}.old-{stamp}", current_exe.display()));
    let staged = PathBuf::from(format!("{}.new-{stamp}", current_exe.display()));

    std::fs::write(&staged, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }

    let replaced = current_exe.exists();
    if replaced {
        std::fs::rename(current_exe, &backup)?;
    }
    if let Err(err) = std::fs::rename(&staged, current_exe) {
        // Put the old binary back before reporting: a half-updated install is
        // worse than a failed update.
        if replaced {
            let _ = std::fs::rename(&backup, current_exe);
        }
        let _ = std::fs::remove_file(&staged);
        return Err(err.into());
    }
    Ok(replaced.then_some(backup))
}

/// Remove backups left behind by earlier updates. Best effort: a locked file
/// (another qc still running) just survives until the next attempt.
pub fn cleanup_backups(current_exe: &Path) {
    let Some(dir) = current_exe.parent() else {
        return;
    };
    let Some(name) = current_exe.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!("{name}.old-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str() else {
            continue;
        };
        if file.starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn read_cache(data_dir: &Path) -> Option<Cache> {
    let text = std::fs::read_to_string(cache_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The cached check, for `qc doctor`: `(checked_at, latest)` when a previous
/// check left an answer behind.
pub fn cached(data_dir: &Path) -> Option<(u64, Option<String>)> {
    read_cache(data_dir).map(|cache| (cache.checked_at, cache.latest))
}

/// Whole days since a cached check, saturating at zero for a clock that moved
/// backwards.
pub fn cache_age_days(checked_at: u64) -> u64 {
    now_secs().saturating_sub(checked_at) / (24 * 60 * 60)
}

fn write_cache(data_dir: &Path, cache: &Cache) {
    if let Err(err) = std::fs::create_dir_all(data_dir) {
        tracing::debug!(%err, "update cache directory");
        return;
    }
    match serde_json::to_string(cache) {
        Ok(text) => {
            if let Err(err) = std::fs::write(cache_path(data_dir), text) {
                tracing::debug!(%err, "update cache write");
            }
        }
        Err(err) => tracing::debug!(%err, "update cache encode"),
    }
}

fn notice_for(latest: &str, current: &str) -> Option<String> {
    is_newer(latest, current)
        .then(|| format!("qcode {latest} is available (this build is {current}); run `qc update`"))
}

/// One stale-check-aware notice, if a newer release exists.
///
/// The answer is cached for `check_days`, so the common case is a file read.
/// Network failures are silent: a version check must never make `qc run` fail.
pub async fn notice(config: &super::config::UpdateConfig, data_dir: &Path) -> Option<String> {
    if !config.enabled || !config.check {
        return None;
    }

    let max_age = config.check_days.saturating_mul(24 * 60 * 60);
    if let Some(cache) = read_cache(data_dir)
        && now_secs().saturating_sub(cache.checked_at) < max_age
    {
        return cache
            .latest
            .as_deref()
            .and_then(|latest| notice_for(latest, CURRENT_VERSION));
    }

    let client = client(NOTICE_TIMEOUT).ok()?;
    let url = release_url(config);
    let body: serde_json::Value = match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => match response.json().await {
            Ok(body) => body,
            Err(err) => {
                tracing::debug!(%err, "update check decode");
                return None;
            }
        },
        Ok(response) => {
            tracing::debug!(status = response.status().as_u16(), "update check status");
            return None;
        }
        Err(err) => {
            tracing::debug!(%err, "update check");
            return None;
        }
    };
    let latest = body
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(|tag| tag.trim_start_matches('v').to_string());
    write_cache(
        data_dir,
        &Cache {
            checked_at: now_secs(),
            latest: latest.clone(),
        },
    );
    latest.and_then(|latest| notice_for(&latest, CURRENT_VERSION))
}

pub async fn run(
    args: UpdateArgs,
    config: &super::config::UpdateConfig,
    data_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let current_exe = match args.install_to.clone() {
        Some(path) => path,
        None => std::env::current_exe()?,
    };
    cleanup_backups(&current_exe);

    println!("current version: {CURRENT_VERSION} ({})", platform());
    let release = match fetch_latest(config).await {
        Ok(release) => release,
        // Not having published anything yet is an answer, not a failure.
        Err(UpdateError::NoRelease { repo }) => {
            println!("no published release for {repo} yet");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    println!("latest release:  {}", release.version);

    if !is_newer(&release.version, CURRENT_VERSION) {
        println!("qcode is up to date");
        return Ok(());
    }
    if args.check {
        println!(
            "update available: {} -> {} (run `qc update` to install)",
            CURRENT_VERSION, release.version
        );
        return Ok(());
    }

    let platform = platform();
    let asset = release
        .asset_for(&platform)
        .ok_or_else(|| UpdateError::NoAsset {
            version: release.version.clone(),
            platform: platform.clone(),
        })?;
    let checksums = release
        .checksums()
        .ok_or_else(|| UpdateError::Unverified(release.version.clone()))?;

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            return Err("refusing to replace the running binary without --yes on a non-TTY".into());
        }
        print!("install {} and restart? [y/N] ", release.version);
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err("cancelled".into());
        }
    }

    println!("downloading {}", asset.url);
    let bytes = download(&asset.url).await?;
    let expected = checksum_for(
        &String::from_utf8_lossy(&download(&checksums.url).await?),
        &asset.name,
    )
    .ok_or_else(|| UpdateError::Manifest(format!("{} missing from SHA256SUMS", asset.name)))?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(UpdateError::Checksum {
            name: asset.name.clone(),
            expected,
            actual,
        }
        .into());
    }

    // Keep the download around so a failed install can be retried by hand.
    if std::fs::create_dir_all(data_dir).is_ok() {
        let _ = std::fs::write(data_dir.join(&asset.name), &bytes);
    }

    let backup = install(&bytes, &current_exe)?;
    match backup {
        Some(backup) => println!(
            "updated to {} (previous binary: {})",
            release.version,
            backup.display()
        ),
        None => println!("installed {} at {}", release.version, current_exe.display()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.1", "0.2.0"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("nonsense", "0.1.0"));
    }

    #[test]
    fn a_prerelease_is_older_than_its_release() {
        assert!(!is_newer("0.2.0-rc1", "0.2.0"));
        assert!(is_newer("0.2.0-rc1", "0.1.9"));
        assert!(is_newer("0.2.0", "0.2.0-rc1"));
    }

    #[test]
    fn assets_are_named_per_platform() {
        assert_eq!(
            asset_name("0.2.0", "linux-x86_64"),
            if cfg!(windows) {
                "qc-0.2.0-linux-x86_64.exe".to_string()
            } else {
                "qc-0.2.0-linux-x86_64".to_string()
            }
        );
    }

    #[test]
    fn checksums_are_found_by_file_name() {
        let sums = "\
deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef  qc-0.2.0-linux-x86_64
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *dist/qc-0.2.0-windows-x86_64.exe
";
        assert_eq!(
            checksum_for(sums, "qc-0.2.0-linux-x86_64").as_deref(),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
        assert_eq!(
            checksum_for(sums, "qc-0.2.0-windows-x86_64.exe").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(checksum_for(sums, "qc-9.9.9-linux-x86_64"), None);
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let long = vec![b'a'; 1000];
        assert_eq!(
            sha256_hex(&long),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn install_swaps_the_binary_and_keeps_a_backup() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = dir.path().join("qc.exe");
        std::fs::write(&exe, b"old-binary").unwrap();

        let backup = install(b"new-binary", &exe)
            .expect("install")
            .expect("backup");
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old-binary");
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".old-")
        );

        cleanup_backups(&exe);
        assert!(!backup.exists(), "backup should be swept on the next run");
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary");
    }

    #[test]
    fn a_fresh_target_is_written_without_a_backup() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = dir.path().join("qc-install.exe");

        let backup = install(b"fresh", &exe).expect("install");
        assert!(backup.is_none());
        assert_eq!(std::fs::read(&exe).unwrap(), b"fresh");
    }

    #[test]
    fn a_stale_cache_only_notices_newer_releases() {
        assert!(notice_for("0.2.0", "0.1.0").is_some());
        assert!(notice_for("0.1.0", "0.1.0").is_none());
        assert!(notice_for("0.0.9", "0.1.0").is_none());
    }

    #[test]
    fn the_cache_round_trips_through_the_data_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_cache(
            dir.path(),
            &Cache {
                checked_at: now_secs(),
                latest: Some("0.2.0".into()),
            },
        );
        let cache = read_cache(dir.path()).expect("cache");
        assert_eq!(cache.latest.as_deref(), Some("0.2.0"));
        assert!(cache.checked_at > 0);
    }
}
