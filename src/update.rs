//! Signed, scheduled self-update with rollback (`sn46-validator update`).
//!
//! Every release carries `manifest.json` (canonical compact sorted JSON plus one newline)
//! and `manifest.sig` (Ed25519 over those exact bytes, 128 hex characters) signed with the
//! offline validator release key. The updater runs as root from a systemd timer: it installs
//! a release only when the signature verifies against the compiled-in key, its sequence is
//! newer than the installed one, its apply-after time plus this host's spread has passed and
//! the host is inside its maintenance window. It keeps the previous binary, restarts the
//! service and rolls back if the service does not stay up. A run interrupted between the swap
//! and the health check is finished by the next one.
//!
//! Its state lives in its own root-owned directory, never the validator's `StateDirectory`,
//! which the unprivileged service user owns.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical::canonical_json;

/// The validator release public key (`release-keys/validator.pub`). Never the miner key.
pub const RELEASE_PUBLIC_KEY: &str =
    "5d57e8737415f87d7920b3c3940b6547fe27ccbd324e97bcbbbdeeebcf5196f9";
pub const MANIFEST_SCHEMA: &str = "sn46.validator.release.v1";
pub const BINARY_NAME: &str = "sn46-validator-linux-x86_64";
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/Subnet46/sn46-validator/releases/latest/download/manifest.json";
pub const DEFAULT_BINARY_URL_BASE: &str =
    "https://github.com/Subnet46/sn46-validator/releases/download/v{version}/";
pub const SERVICE: &str = "sn46-validator";
const USER_AGENT: &str = concat!("sn46-validator/", env!("CARGO_PKG_VERSION"));
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const HEALTH_POLL: Duration = Duration::from_secs(2);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("Update configuration is invalid: {0}")]
    Config(String),
    #[error("Update fetch failed: {0}")]
    Fetch(String),
    #[error("Release manifest rejected: {0}")]
    Manifest(String),
    #[error("Downloaded binary rejected: {0}")]
    Binary(String),
    /// The signed binary downloaded intact but does not run as that version on this host;
    /// its sequence is not retried.
    #[error("Release binary is unusable here: {0}")]
    BadRelease(String),
    #[error(
        "Not enough disk space to update: need {needed_bytes} bytes, {available_bytes} available"
    )]
    Disk {
        needed_bytes: u64,
        available_bytes: u64,
    },
    #[error("Release {version} failed its health check ({reason}); rolled back")]
    RolledBack { version: String, reason: String },
    #[error(
        "Release {version} failed its health check ({reason}); rollback incomplete, retried next run"
    )]
    RollbackIncomplete { version: String, reason: String },
    #[error("Update I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub apply_after_ms: u64,
    pub binary: BinaryEntry,
    pub schema: String,
    pub sequence: u64,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BinaryEntry {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

/// A UTC maintenance window in minutes of the day; `start > end` wraps midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    start: u32,
    end: u32,
}

impl Window {
    pub fn parse(text: &str) -> Result<Self, UpdateError> {
        let invalid = || UpdateError::Config(format!("UPDATE_WINDOW {text:?} is not HH:MM-HH:MM"));
        let minutes = |part: &str| -> Option<u32> {
            let (hours, minutes) = part.trim().split_once(':')?;
            if hours.len() != 2 || minutes.len() != 2 {
                return None;
            }
            let (hours, minutes): (u32, u32) = (hours.parse().ok()?, minutes.parse().ok()?);
            (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
        };
        let (start, end) = text.split_once('-').ok_or_else(invalid)?;
        let (start, end) = (
            minutes(start).ok_or_else(invalid)?,
            minutes(end).ok_or_else(invalid)?,
        );
        if start == end {
            return Err(invalid());
        }
        Ok(Self { start, end })
    }

    pub fn contains(&self, now_ms: u64) -> bool {
        let minute = ((now_ms / 60_000) % 1440) as u32;
        if self.start < self.end {
            (self.start..self.end).contains(&minute)
        } else {
            minute >= self.start || minute < self.end
        }
    }
}

pub struct Config {
    pub enabled: bool,
    pub now: bool,
    pub window: Option<Window>,
    pub spread_s: u64,
    pub manifest_url: String,
    pub binary_url_base: String,
    pub install_path: PathBuf,
    pub state_dir: PathBuf,
    pub systemctl: String,
    pub health: Duration,
    /// Seeds this host's spread: the hotkey address, else the machine ID, else the hostname.
    pub host_id: String,
    pub public_key: VerifyingKey,
    pub running_version: String,
    /// Free bytes on the install directory's filesystem (a seam for tests).
    pub available_bytes: fn(&Path) -> Result<u64, UpdateError>,
}

impl Config {
    /// Read `AUTO_UPDATE`, `UPDATE_*` and `SYSTEMCTL` through `lookup` (the environment in
    /// production). `hotkey_file` is the wallet's hotkey file, read only for its address.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        now: bool,
        hotkey_file: Option<&Path>,
    ) -> Result<Self, UpdateError> {
        let get = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let seconds = |key: &str, default: u64| -> Result<u64, UpdateError> {
            get(key).map_or(Ok(default), |value| {
                value
                    .parse()
                    .map_err(|_| UpdateError::Config(format!("{key} must be whole seconds")))
            })
        };
        let enabled = !matches!(
            get("AUTO_UPDATE").as_deref(),
            Some("0" | "false" | "no" | "off")
        );
        let host_id = hotkey_file
            .and_then(hotkey_address)
            .or_else(|| read_trimmed("/etc/machine-id"))
            .or_else(|| read_trimmed("/proc/sys/kernel/hostname"))
            .unwrap_or_default();
        Ok(Self {
            enabled,
            now,
            window: get("UPDATE_WINDOW")
                .map(|w| Window::parse(&w))
                .transpose()?,
            spread_s: seconds("UPDATE_SPREAD_S", 7200)?,
            manifest_url: get("UPDATE_MANIFEST_URL").unwrap_or(DEFAULT_MANIFEST_URL.into()),
            binary_url_base: get("UPDATE_BINARY_URL_BASE")
                .unwrap_or(DEFAULT_BINARY_URL_BASE.into()),
            install_path: get("UPDATE_INSTALL_PATH")
                .unwrap_or("/usr/local/bin/sn46-validator".into())
                .into(),
            state_dir: get("UPDATE_STATE_DIR")
                .unwrap_or("/var/lib/sn46-validator-update".into())
                .into(),
            systemctl: get("SYSTEMCTL").unwrap_or("systemctl".into()),
            health: Duration::from_secs(seconds("UPDATE_HEALTH_S", 30)?),
            host_id,
            public_key: compiled_public_key(),
            running_version: env!("CARGO_PKG_VERSION").into(),
            available_bytes,
        })
    }
}

pub fn compiled_public_key() -> VerifyingKey {
    let bytes: [u8; 32] = hex::decode(RELEASE_PUBLIC_KEY)
        .expect("release key is hex")
        .try_into()
        .expect("release key is 32 bytes");
    VerifyingKey::from_bytes(&bytes).expect("release key is a valid point")
}

fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// The `ss58Address` of a bittensor hotkey file; the secret fields are ignored.
fn hotkey_address(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Hotkey {
        #[serde(rename = "ss58Address")]
        ss58_address: String,
    }
    let raw = fs::read(path).ok()?;
    serde_json::from_slice::<Hotkey>(&raw)
        .ok()
        .map(|hotkey| hotkey.ss58_address)
}

/// Verify `manifest.sig` over the exact `manifest.json` bytes, then that the bytes are the
/// canonical encoding and the fields are well formed.
pub fn verify_manifest(
    raw: &[u8],
    signature_hex: &[u8],
    key: &VerifyingKey,
) -> Result<Manifest, UpdateError> {
    let reject = |why: &str| UpdateError::Manifest(why.into());
    let signature_hex = std::str::from_utf8(signature_hex).map_err(|_| reject("bad signature"))?;
    let signature_hex = signature_hex.strip_suffix('\n').unwrap_or(signature_hex);
    if signature_hex.len() != 128 {
        return Err(reject("signature must be 128 hex characters"));
    }
    let signature = hex::decode(signature_hex)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or_else(|| reject("signature is not hex"))?;
    key.verify_strict(raw, &signature)
        .map_err(|_| reject("Ed25519 signature does not verify with the release key"))?;
    let value: serde_json::Value =
        serde_json::from_slice(raw).map_err(|_| reject("manifest is not JSON"))?;
    let mut canonical = canonical_json(&value).into_bytes();
    canonical.push(b'\n');
    if canonical != raw {
        return Err(reject(
            "manifest is not canonical compact sorted JSON with one trailing newline",
        ));
    }
    let manifest: Manifest =
        serde_json::from_value(value).map_err(|error| reject(&format!("fields: {error}")))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(reject("unsupported manifest schema"));
    }
    let parts: Vec<_> = manifest.version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(reject("version must be X.Y.Z"));
    }
    if manifest.binary.name != BINARY_NAME {
        return Err(reject("unexpected binary name"));
    }
    if manifest.binary.sha256.len() != 64
        || !manifest
            .binary
            .sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(reject("binary sha256 must be 64 lowercase hex characters"));
    }
    if manifest.binary.size == 0 || manifest.binary.size > MAX_BINARY_BYTES {
        return Err(reject("binary size is out of range"));
    }
    Ok(manifest)
}

/// This host's deterministic delay after `apply_after_ms`, in `[0, spread_s)` seconds.
pub fn spread_seconds(host_id: &str, spread_s: u64) -> u64 {
    if spread_s == 0 {
        return 0;
    }
    let digest = Sha256::digest(host_id.as_bytes());
    let head: [u8; 8] = digest[..8].try_into().expect("8 bytes");
    u64::from_be_bytes(head) % spread_s
}

/// What `release.json` records: the release the updater (or installer) put in place.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub sequence: u64,
}

/// What `pending.json` records from just before the swap until the release is recorded or
/// the previous binary is back and running, so an interrupted run is finished by the next.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Pending {
    version: String,
    sequence: u64,
    #[serde(default)]
    rolling_back: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct FailedReleases {
    failed: Vec<FailedRelease>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FailedRelease {
    sequence: u64,
    version: String,
    failed_ms: u64,
}

/// Why a run ended without an error.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Disabled,
    Busy,
    NoManifest,
    NotNewer,
    AlreadyRunning,
    PreviouslyFailed,
    Waiting { until_ms: u64 },
    OutsideWindow,
    Updated { version: String },
}

/// One updater run. `now_ms` is the wall clock (injected for tests).
pub fn run(config: &Config, now_ms: u64) -> Result<Outcome, UpdateError> {
    if !config.enabled {
        tracing::info!("Auto-update is disabled (AUTO_UPDATE=0); nothing to do");
        return Ok(Outcome::Disabled);
    }
    fs::create_dir_all(&config.state_dir)?;
    check_state_dir(&config.state_dir)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(config.state_dir.join("update.lock"))?;
    // A few seconds' grace: a child forked elsewhere in this process can briefly share
    // the descriptor before it execs.
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(fs::TryLockError::WouldBlock) => {
                tracing::info!("Another update is running; skipping");
                return Ok(Outcome::Busy);
            }
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }

    let pending_path = config.state_dir.join("pending.json");
    if let Some(pending) = read_json::<Pending>(&pending_path)? {
        // `.previous` exists before `pending.json` is written and only a rollback moves it
        // away, so its absence while the old version runs means a rollback restored it even
        // if it could not mark that in `pending.json`.
        let restored = pending.version != config.running_version
            && !previous_path(&config.install_path).exists();
        if pending.rolling_back || restored {
            tracing::warn!("Finishing the interrupted rollback of {}", pending.version);
            let reason = "an earlier rollback was interrupted".into();
            return Err(roll_back(config, &pending, now_ms, reason));
        }
        if pending.version == config.running_version {
            tracing::warn!(
                "Finishing the interrupted update to {} (sequence {})",
                pending.version,
                pending.sequence
            );
            return confirm(config, &pending, now_ms);
        }
        // The swap never landed.
        fs::remove_file(&pending_path)?;
    }

    let installed: Installed =
        read_json(&config.state_dir.join("release.json"))?.unwrap_or_default();
    let Some(raw) = fetch(&config.manifest_url, MAX_MANIFEST_BYTES)? else {
        tracing::info!(url = %config.manifest_url, "No signed release manifest published; skipping");
        return Ok(Outcome::NoManifest);
    };
    let signature_url = format!("{}.sig", config.manifest_url.trim_end_matches(".json"));
    let signature = fetch(&signature_url, 1024)?
        .ok_or_else(|| UpdateError::Fetch(format!("{signature_url} is missing")))?;
    let manifest = verify_manifest(&raw, &signature, &config.public_key)?;
    let (version, sequence) = (&manifest.version, manifest.sequence);

    if sequence <= installed.sequence {
        tracing::info!(
            "Skipped: release {version} sequence {sequence} is not newer than installed sequence {}",
            installed.sequence
        );
        return Ok(Outcome::NotNewer);
    }
    if *version == config.running_version {
        write_json(
            &config.state_dir.join("release.json"),
            &Installed {
                version: version.clone(),
                sequence,
            },
        )?;
        tracing::info!("Skipped: already running {version}; recorded sequence {sequence}");
        return Ok(Outcome::AlreadyRunning);
    }
    let failed: FailedReleases =
        read_json(&config.state_dir.join("update-failed.json"))?.unwrap_or_default();
    if failed.failed.iter().any(|f| f.sequence == sequence) {
        tracing::info!(
            "Skipped: release {version} sequence {sequence} failed here before and is not retried"
        );
        return Ok(Outcome::PreviouslyFailed);
    }
    let spread = spread_seconds(&config.host_id, config.spread_s);
    let due_ms = manifest
        .apply_after_ms
        .saturating_add(spread.saturating_mul(1000));
    if config.now {
        tracing::info!("--now: ignoring the apply-after time and the maintenance window");
    } else if now_ms < due_ms {
        tracing::info!(
            "Skipped: release {version} is waiting until {} (apply-after {} plus this host's spread of {spread}s)",
            format_utc(due_ms),
            format_utc(manifest.apply_after_ms)
        );
        return Ok(Outcome::Waiting { until_ms: due_ms });
    } else if let Some(window) = config.window.filter(|w| !w.contains(now_ms)) {
        tracing::info!(
            "Skipped: release {version} is due but {} is outside UPDATE_WINDOW {:02}:{:02}-{:02}:{:02} UTC",
            format_utc(now_ms),
            window.start / 60,
            window.start % 60,
            window.end / 60,
            window.end % 60
        );
        return Ok(Outcome::OutsideWindow);
    }
    tracing::info!(
        "⬇️ Updating {} -> {version} (sequence {sequence})",
        config.running_version
    );

    let install_dir = config
        .install_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !config.install_path.is_file() {
        return Err(UpdateError::Config(format!(
            "{} is not installed; nothing to update",
            config.install_path.display()
        )));
    }
    check_disk(config, install_dir, &manifest)?;
    let staged = install_dir.join(".sn46-validator.new");
    if let Err(error) = stage(config, &manifest, &staged) {
        let _ = fs::remove_file(&staged);
        if matches!(error, UpdateError::BadRelease(_)) {
            record_failure(config, version, sequence, now_ms)?;
        }
        return Err(error);
    }

    // `.previous` must be durable before `pending.json` names the release: recovery reads
    // its absence as "a rollback already restored it".
    let previous = previous_path(&config.install_path);
    if let Err(error) =
        fs::copy(&config.install_path, &previous).and_then(|_| File::open(&previous)?.sync_all())
    {
        let _ = fs::remove_file(&staged);
        return Err(error.into());
    }
    sync_dir(install_dir);
    let pending = Pending {
        version: version.clone(),
        sequence,
        rolling_back: false,
    };
    if let Err(error) = write_json(&pending_path, &pending) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    if let Err(error) = fs::rename(&staged, &config.install_path) {
        let _ = fs::remove_file(&staged);
        let _ = fs::remove_file(&pending_path);
        return Err(error.into());
    }
    sync_dir(install_dir);
    tracing::info!("Installed {version}; restarting {SERVICE}");
    confirm(config, &pending, now_ms)
}

/// With the pending release swapped in: restart the service, then record the release, or
/// put the previous binary back and never retry it.
fn confirm(config: &Config, pending: &Pending, now_ms: u64) -> Result<Outcome, UpdateError> {
    let (version, sequence) = (&pending.version, pending.sequence);
    let pending_path = config.state_dir.join("pending.json");
    match restart_and_check(config) {
        Ok(()) => {
            write_json(
                &config.state_dir.join("release.json"),
                &Installed {
                    version: version.into(),
                    sequence,
                },
            )?;
            fs::remove_file(&pending_path)?;
            tracing::info!("✅ Updated to {version} (sequence {sequence}); {SERVICE} is healthy");
            Ok(Outcome::Updated {
                version: version.into(),
            })
        }
        Err(reason) => {
            tracing::error!("❌ {version} failed its health check: {reason}; rolling back");
            Err(roll_back(config, pending, now_ms, reason))
        }
    }
}

/// Put the previous binary back and restart it. Bookkeeping failures are logged, never
/// allowed to stop the restore; `pending.json` stays, marked as a rollback, until the
/// previous version restarts and the failed sequence is recorded, so a later run finishes
/// an incomplete one instead of reinstalling the bad release.
fn roll_back(config: &Config, pending: &Pending, now_ms: u64, reason: String) -> UpdateError {
    let version = pending.version.clone();
    let pending_path = config.state_dir.join("pending.json");
    let marker = Pending {
        rolling_back: true,
        ..pending.clone()
    };
    if let Err(error) = write_json(&pending_path, &marker) {
        tracing::error!("Cannot mark the rollback of {version} in progress: {error}");
    }
    let recorded = record_failure(config, &version, pending.sequence, now_ms)
        .inspect_err(|error| tracing::error!("Cannot record {version} as failed: {error}"))
        .is_ok();
    let previous = previous_path(&config.install_path);
    // Already gone when an earlier, interrupted rollback restored it.
    if previous.exists() {
        if let Err(error) = fs::rename(&previous, &config.install_path) {
            tracing::error!("Cannot restore the previous binary: {error}");
            return UpdateError::RollbackIncomplete { version, reason };
        }
        if let Some(dir) = config.install_path.parent() {
            sync_dir(dir);
        }
    }
    if let Err(error) = systemctl(config, &["restart", SERVICE]) {
        tracing::error!("Restart after rollback failed: {error}; the next run retries it");
        return UpdateError::RollbackIncomplete { version, reason };
    }
    if !recorded {
        return UpdateError::RollbackIncomplete { version, reason };
    }
    if let Err(error) = fs::remove_file(&pending_path)
        && error.kind() != ErrorKind::NotFound
    {
        tracing::error!("Cannot clear {}: {error}", pending_path.display());
    }
    UpdateError::RolledBack { version, reason }
}

/// Never retry `sequence` here. Recording it twice is a no-op.
fn record_failure(
    config: &Config,
    version: &str,
    sequence: u64,
    now_ms: u64,
) -> Result<(), UpdateError> {
    let path = config.state_dir.join("update-failed.json");
    let mut failed: FailedReleases = read_json(&path)?.unwrap_or_default();
    if failed.failed.iter().any(|f| f.sequence == sequence) {
        return Ok(());
    }
    failed.failed.push(FailedRelease {
        sequence,
        version: version.into(),
        failed_ms: now_ms,
    });
    write_json(&path, &failed)
}

/// The updater runs as root and writes here, so only its own user may be able to.
fn check_state_dir(dir: &Path) -> Result<(), UpdateError> {
    let meta = fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != euid || meta.mode() & 0o022 != 0 {
        return Err(UpdateError::Config(format!(
            "{} must be a directory owned by uid {euid} and writable only by it",
            dir.display()
        )));
    }
    Ok(())
}

pub fn previous_path(install_path: &Path) -> PathBuf {
    install_path.with_added_extension("previous")
}

fn check_disk(config: &Config, dir: &Path, manifest: &Manifest) -> Result<(), UpdateError> {
    let status = config.state_dir.join("update-status.json");
    let needed_bytes = manifest.binary.size.saturating_mul(3);
    let available_bytes = (config.available_bytes)(dir)?;
    if available_bytes < needed_bytes {
        tracing::warn!(
            needed_bytes,
            available_bytes,
            "⚠️ Update to {} blocked: not enough free space in {}",
            manifest.version,
            dir.display()
        );
        write_json(
            &status,
            &serde_json::json!({
                "blocked": "disk",
                "needed_bytes": needed_bytes,
                "available_bytes": available_bytes,
                "version": manifest.version,
                "sequence": manifest.sequence,
            }),
        )?;
        return Err(UpdateError::Disk {
            needed_bytes,
            available_bytes,
        });
    }
    match fs::remove_file(&status) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Free bytes for an unprivileged writer on `dir`'s filesystem.
pub fn available_bytes(dir: &Path) -> Result<u64, UpdateError> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| UpdateError::Config("install path contains NUL".into()))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is a valid C string and `stat` is a writable statvfs buffer.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: statvfs returned 0, so it filled the buffer.
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// Download, verify and test-run the new binary at `staged`.
fn stage(config: &Config, manifest: &Manifest, staged: &Path) -> Result<(), UpdateError> {
    let url = format!(
        "{}{}",
        config
            .binary_url_base
            .replace("{version}", &manifest.version),
        manifest.binary.name
    );
    let binary = fetch(&url, manifest.binary.size)?
        .ok_or_else(|| UpdateError::Fetch(format!("{url} is missing")))?;
    if binary.len() as u64 != manifest.binary.size {
        return Err(UpdateError::Binary(format!(
            "size {} does not match the manifest's {}",
            binary.len(),
            manifest.binary.size
        )));
    }
    if hex::encode(Sha256::digest(&binary)) != manifest.binary.sha256 {
        return Err(UpdateError::Binary(
            "sha256 does not match the manifest".into(),
        ));
    }
    let _ = fs::remove_file(staged);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(staged)?;
    file.write_all(&binary)?;
    file.set_permissions(fs::Permissions::from_mode(0o755))?;
    file.sync_all()?;
    drop(file);
    let expected = format!("sn46-validator {}", manifest.version);
    let reported = binary_version(staged).map_err(|error| match error {
        UpdateError::Binary(why) => UpdateError::BadRelease(why),
        other => other,
    })?;
    if reported != expected {
        return Err(UpdateError::BadRelease(format!(
            "--version printed {reported:?}, expected {expected:?}"
        )));
    }
    Ok(())
}

fn binary_version(path: &Path) -> Result<String, UpdateError> {
    let mut command = Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child =
        spawn(&mut command).map_err(|error| UpdateError::Binary(format!("cannot run: {error}")))?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            let mut out = String::new();
            if let Some(stdout) = child.stdout.take() {
                stdout.take(4096).read_to_string(&mut out).ok();
            }
            if !status.success() {
                return Err(UpdateError::Binary(format!(
                    "--version exited with {status}"
                )));
            }
            return Ok(out.trim().to_owned());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdateError::Binary("--version did not finish".into()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn, retrying briefly on ETXTBSY: a just-written executable can still be open for
/// writing in a child forked concurrently by another thread.
fn spawn(command: &mut Command) -> std::io::Result<std::process::Child> {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

fn systemctl(config: &Config, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(&config.systemctl);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let output = spawn(&mut command)
        .and_then(|child| child.wait_with_output())
        .map_err(|error| format!("{} {}: {error}", config.systemctl, args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "{} {} exited with {}",
            config.systemctl,
            args.join(" "),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `(ActiveState, NRestarts)` of the validator service.
fn unit_state(config: &Config) -> Result<(String, u64), String> {
    let text = systemctl(config, &["show", "-p", "ActiveState,NRestarts", SERVICE])?;
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .ok_or_else(|| format!("systemctl show did not report {key}"))
    };
    let restarts = field("NRestarts")?
        .parse()
        .map_err(|_| "NRestarts is not a number".to_owned())?;
    Ok((field("ActiveState")?.to_owned(), restarts))
}

/// Restart the service and require it to stay active, without automatic restarts, for the
/// health period.
fn restart_and_check(config: &Config) -> Result<(), String> {
    systemctl(config, &["restart", SERVICE])?;
    let deadline = Instant::now() + config.health;
    let (_, baseline) = unit_state(config)?;
    loop {
        let (state, restarts) = unit_state(config)?;
        if state != "active" {
            return Err(format!("{SERVICE} is {state}"));
        }
        if restarts != baseline {
            return Err(format!("{SERVICE} restarted ({baseline} -> {restarts})"));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep(HEALTH_POLL.min(deadline - now));
    }
}

/// Fetch `url` (https:// or file://), at most `max` bytes. `None` when it does not exist.
fn fetch(url: &str, max: u64) -> Result<Option<Vec<u8>>, UpdateError> {
    let too_big = || UpdateError::Fetch(format!("{url} is larger than {max} bytes"));
    if let Some(path) = url.strip_prefix("file://") {
        let mut raw = Vec::new();
        match File::open(path) {
            Ok(file) => file.take(max + 1).read_to_end(&mut raw)?,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        return if raw.len() as u64 > max {
            Err(too_big())
        } else {
            Ok(Some(raw))
        };
    }
    if !url.starts_with("https://") {
        return Err(UpdateError::Config(format!(
            "{url} must be an https:// or file:// URL"
        )));
    }
    // Follows GitHub's releases/latest and releases/download redirects (ureq's default of 10).
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .user_agent(USER_AGENT)
        .https_only(true)
        .http_status_as_error(true)
        .build()
        .new_agent();
    let mut raw = Vec::new();
    match agent.get(url).call() {
        Ok(response) => {
            response
                .into_body()
                .into_reader()
                .take(max + 1)
                .read_to_end(&mut raw)
                .map_err(|error| UpdateError::Fetch(format!("{url}: {error}")))?;
        }
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(error) => return Err(UpdateError::Fetch(format!("{url}: {error}"))),
    }
    if raw.len() as u64 > max {
        return Err(too_big());
    }
    Ok(Some(raw))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, UpdateError> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).map(Some).map_err(|error| {
            UpdateError::Config(format!("{} is malformed: {error}", path.display()))
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), UpdateError> {
    let temporary = path.with_added_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(canonical_json(value).as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
    Ok(())
}

fn sync_dir(dir: &Path) {
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time in milliseconds.
pub fn format_utc(ms: u64) -> String {
    let seconds = ms / 1000;
    let (days, rest) = ((seconds / 86400) as i64, seconds % 86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&KEY)
    }

    fn manifest_value(version: &str, sequence: u64, binary: &[u8]) -> serde_json::Value {
        json!({
            "apply_after_ms": 0,
            "binary": {
                "name": BINARY_NAME,
                "sha256": hex::encode(Sha256::digest(binary)),
                "size": binary.len(),
            },
            "schema": MANIFEST_SCHEMA,
            "sequence": sequence,
            "version": version,
        })
    }

    fn encode(value: &serde_json::Value) -> Vec<u8> {
        let mut raw = canonical_json(value).into_bytes();
        raw.push(b'\n');
        raw
    }

    fn sign(raw: &[u8]) -> Vec<u8> {
        format!("{}\n", hex::encode(signing_key().sign(raw).to_bytes())).into_bytes()
    }

    #[test]
    fn compiled_key_is_the_validator_release_key() {
        assert_eq!(
            hex::encode(compiled_public_key().to_bytes()),
            RELEASE_PUBLIC_KEY
        );
    }

    #[test]
    fn signed_canonical_manifest_verifies() {
        let raw = encode(&manifest_value("0.1.4", 3, b"binary"));
        let manifest = verify_manifest(&raw, &sign(&raw), &signing_key().verifying_key()).unwrap();
        assert_eq!(manifest.version, "0.1.4");
        assert_eq!(manifest.sequence, 3);
        assert_eq!(manifest.binary.size, 6);
    }

    #[test]
    fn bad_signature_wrong_key_and_tampering_are_rejected() {
        let key = signing_key().verifying_key();
        let raw = encode(&manifest_value("0.1.4", 3, b"binary"));
        let mut signature = sign(&raw);
        signature[0] = if signature[0] == b'0' { b'1' } else { b'0' };
        assert!(verify_manifest(&raw, &signature, &key).is_err());
        assert!(verify_manifest(&raw, &sign(&raw), &compiled_public_key()).is_err());
        assert!(verify_manifest(&raw, b"zz", &key).is_err());
        let tampered = encode(&manifest_value("0.1.4", 4, b"binary"));
        assert!(verify_manifest(&tampered, &sign(&raw), &key).is_err());
    }

    #[test]
    fn non_canonical_bytes_are_rejected_even_when_signed() {
        let key = signing_key().verifying_key();
        let value = manifest_value("0.1.4", 3, b"binary");
        let pretty = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        let err = verify_manifest(pretty.as_bytes(), &sign(pretty.as_bytes()), &key).unwrap_err();
        assert!(err.to_string().contains("canonical"), "{err}");
        let bare = canonical_json(&value).into_bytes();
        assert!(verify_manifest(&bare, &sign(&bare), &key).is_err());
    }

    #[test]
    fn wrong_schema_fields_or_version_are_rejected() {
        let key = signing_key().verifying_key();
        let mut cases = Vec::new();
        let mut schema = manifest_value("0.1.4", 3, b"binary");
        schema["schema"] = json!("sn46.worker.release-index.v2");
        cases.push(schema);
        let mut extra = manifest_value("0.1.4", 3, b"binary");
        extra["url"] = json!("https://example.com");
        cases.push(extra);
        cases.push(manifest_value("0.1.4-rc1", 3, b"binary"));
        cases.push(manifest_value("../x", 3, b"binary"));
        let mut name = manifest_value("0.1.4", 3, b"binary");
        name["binary"]["name"] = json!("../../etc/passwd");
        cases.push(name);
        for value in cases {
            let raw = encode(&value);
            assert!(verify_manifest(&raw, &sign(&raw), &key).is_err(), "{value}");
        }
    }

    #[test]
    fn spread_is_deterministic_and_bounded() {
        assert_eq!(spread_seconds("5Fhotkey", 0), 0);
        let a = spread_seconds("5Fhotkey", 7200);
        assert_eq!(a, spread_seconds("5Fhotkey", 7200));
        assert!(a < 7200);
        let spreads: std::collections::BTreeSet<_> = (0..50)
            .map(|i| spread_seconds(&format!("host{i}"), 7200))
            .collect();
        assert!(spreads.len() > 40, "hosts should spread out");
        assert!(spreads.iter().all(|s| *s < 7200));
    }

    #[test]
    fn windows_parse_and_wrap_midnight() {
        let at = |h: u64, m: u64| (h * 60 + m) * 60_000 + 86_400_000 * 20_000;
        let day = Window::parse("02:00-05:00").unwrap();
        assert!(day.contains(at(2, 0)) && day.contains(at(4, 59)));
        assert!(!day.contains(at(5, 0)) && !day.contains(at(1, 59)));
        let night = Window::parse("22:30-01:15").unwrap();
        assert!(night.contains(at(22, 30)) && night.contains(at(23, 59)));
        assert!(night.contains(at(0, 0)) && night.contains(at(1, 14)));
        assert!(!night.contains(at(1, 15)) && !night.contains(at(12, 0)));
        for bad in [
            "",
            "2-5",
            "02:00",
            "24:00-01:00",
            "02:60-03:00",
            "03:00-03:00",
        ] {
            assert!(Window::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_790_000_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn hotkey_address_is_read_from_the_wallet_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("default");
        fs::write(
            &path,
            r#"{"ss58Address":"5FnoWRL4FYzd29q8zXoY3JWio9PtBPgrAFu2RcgQazRQsiCs","secretSeed":"0x42"}"#,
        )
        .unwrap();
        let config = Config::from_lookup(|_| None, false, Some(&path)).unwrap();
        assert_eq!(
            config.host_id,
            "5FnoWRL4FYzd29q8zXoY3JWio9PtBPgrAFu2RcgQazRQsiCs"
        );
    }
}
