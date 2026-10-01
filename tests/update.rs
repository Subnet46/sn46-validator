//! The updater end to end: a file:// release signed with a test key, a fake `systemctl` that
//! logs its calls, and the real swap, rollback and state files. The last tests run the
//! binary as the timer does, against the compiled-in validator release key.
//!
//! These spawn processes, so they live outside the library's unit tests: a child forked
//! mid-test would briefly share those tests' lock descriptors.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sn46_validator::canonical::canonical_json;
use sn46_validator::update::{
    BINARY_NAME, Config, MANIFEST_SCHEMA, Outcome, UpdateError, Window, run, spread_seconds,
};

const NOW: u64 = 1_790_000_000_000;

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn fake_binary(version: &str) -> Vec<u8> {
    format!("#!/usr/bin/env bash\necho \"sn46-validator {version}\"\n").into_bytes()
}

fn manifest(version: &str, sequence: u64, apply_after_ms: u64, binary: &[u8]) -> Vec<u8> {
    let value = json!({
        "apply_after_ms": apply_after_ms,
        "binary": {
            "name": BINARY_NAME,
            "sha256": hex::encode(Sha256::digest(binary)),
            "size": binary.len(),
        },
        "schema": MANIFEST_SCHEMA,
        "sequence": sequence,
        "version": version,
    });
    let mut raw = canonical_json(&value).into_bytes();
    raw.push(b'\n');
    raw
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A release directory, a state directory, an installed 0.1.3 and a fake systemctl.
struct Host {
    dir: tempfile::TempDir,
    config: Config,
}

impl Host {
    /// `restarts` is the shell expression the fake systemctl reports as NRestarts, with `n`
    /// the number of `show` calls so far.
    fn new(restarts: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for sub in ["release", "state", "bin"] {
            fs::create_dir(root.join(sub)).unwrap();
        }
        fs::write(root.join("bin/sn46-validator"), fake_binary("0.1.3")).unwrap();
        fs::set_permissions(
            root.join("bin/sn46-validator"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        script(
            &root.join("systemctl"),
            &format!(
                "set -eu\nlog={log}\necho \"$*\" >> \"$log\"\n\
                 case \"$1\" in\n  restart) ;;\n  show) n=$(grep -c '^show' \"$log\"); \
                 echo ActiveState=active; echo NRestarts=$(({restarts}));;\n  *) exit 1;;\nesac\n",
                log = root.join("systemctl.log").display(),
            ),
        );
        let lookup = |key: &str| {
            Some(match key {
                "UPDATE_MANIFEST_URL" => format!("file://{}/release/manifest.json", root.display()),
                "UPDATE_BINARY_URL_BASE" => format!("file://{}/release/", root.display()),
                "UPDATE_INSTALL_PATH" => format!("{}/bin/sn46-validator", root.display()),
                "UPDATE_STATE_DIR" => format!("{}/state", root.display()),
                "SYSTEMCTL" => format!("{}/systemctl", root.display()),
                "UPDATE_HEALTH_S" => "0".into(),
                _ => return None,
            })
        };
        let mut config = Config::from_lookup(lookup, false, None).unwrap();
        config.public_key = signing_key().verifying_key();
        config.running_version = "0.1.3".into();
        config.host_id = "5Ftesthost".into();
        Self { dir, config }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Publish `binary` under a manifest for `version`, signed with the test key.
    fn publish_binary(&self, version: &str, sequence: u64, apply_after_ms: u64, binary: &[u8]) {
        let raw = manifest(version, sequence, apply_after_ms, binary);
        let signature = hex::encode(signing_key().sign(&raw).to_bytes());
        fs::write(self.path("release").join(BINARY_NAME), binary).unwrap();
        fs::write(self.path("release/manifest.json"), &raw).unwrap();
        fs::write(self.path("release/manifest.sig"), signature + "\n").unwrap();
    }

    fn publish(&self, version: &str, sequence: u64, apply_after_ms: u64) {
        self.publish_binary(version, sequence, apply_after_ms, &fake_binary(version));
    }

    fn json(&self, name: &str) -> Option<Value> {
        fs::read(self.path(name))
            .ok()
            .map(|raw| serde_json::from_slice(&raw).unwrap())
    }

    fn installed(&self) -> Option<Value> {
        self.json("state/release.json")
    }

    fn assert_installed(&self, version: &str) {
        assert_eq!(
            fs::read(self.path("bin/sn46-validator")).unwrap(),
            fake_binary(version)
        );
    }

    fn systemctl_log(&self) -> String {
        fs::read_to_string(self.path("systemctl.log")).unwrap_or_default()
    }
}

#[test]
fn disabled_does_nothing() {
    let mut host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    host.config.enabled = false;
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::Disabled);
    assert!(!host.path("state/update.lock").exists());
    let off = Config::from_lookup(|k| (k == "AUTO_UPDATE").then(|| "0".into()), false, None);
    assert!(!off.unwrap().enabled);
    assert!(Config::from_lookup(|_| None, false, None).unwrap().enabled);
}

#[test]
fn missing_manifest_is_a_quiet_skip() {
    let host = Host::new("0");
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::NoManifest);
}

#[test]
fn end_to_end_update_swaps_keeps_previous_and_records_the_release() {
    let host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    assert_eq!(
        run(&host.config, NOW).unwrap(),
        Outcome::Updated {
            version: "0.1.4".into()
        }
    );
    host.assert_installed("0.1.4");
    assert_eq!(
        fs::read(host.path("bin/sn46-validator.previous")).unwrap(),
        fake_binary("0.1.3")
    );
    let mode = fs::metadata(host.path("bin/sn46-validator"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(!host.path("bin/.sn46-validator.new").exists());
    assert_eq!(
        host.installed(),
        Some(json!({"sequence": 2, "version": "0.1.4"}))
    );
    assert!(!host.path("state/pending.json").exists());
    let log = host.systemctl_log();
    assert!(log.starts_with("restart sn46-validator\n"), "{log}");
    assert!(log.contains("show -p ActiveState,NRestarts sn46-validator\n"));
    // The same release again is a replay.
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::NotNewer);
}

#[test]
fn replay_and_downgrade_are_rejected() {
    let host = Host::new("0");
    fs::write(
        host.path("state/release.json"),
        r#"{"sequence":5,"version":"0.1.3"}"#,
    )
    .unwrap();
    host.publish("0.1.4", 5, 0);
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::NotNewer);
    host.publish("0.1.2", 4, 0);
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::NotNewer);
    host.assert_installed("0.1.3");
    assert!(host.systemctl_log().is_empty());
}

#[test]
fn the_running_version_only_records_its_sequence() {
    let host = Host::new("0");
    host.publish("0.1.3", 7, 0);
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::AlreadyRunning);
    assert_eq!(host.installed().unwrap()["sequence"], 7);
    assert!(host.systemctl_log().is_empty());
}

#[test]
fn apply_after_plus_spread_and_the_window_gate_the_update() {
    let mut host = Host::new("0");
    host.config.spread_s = 7200;
    let spread_ms = spread_seconds("5Ftesthost", 7200) * 1000;
    assert!(spread_ms > 0, "pick a host id with a nonzero spread");
    host.publish("0.1.4", 2, NOW);
    let until_ms = NOW + spread_ms;
    assert_eq!(
        run(&host.config, NOW).unwrap(),
        Outcome::Waiting { until_ms }
    );
    assert_eq!(
        run(&host.config, until_ms - 1).unwrap(),
        Outcome::Waiting { until_ms }
    );
    // Due, but outside a window that starts an hour later.
    let minute = (until_ms / 60_000) % 1440;
    let (start, end) = ((minute + 60) % 1440, (minute + 120) % 1440);
    let window = format!(
        "{:02}:{:02}-{:02}:{:02}",
        start / 60,
        start % 60,
        end / 60,
        end % 60
    );
    host.config.window = Some(Window::parse(&window).unwrap());
    assert_eq!(run(&host.config, until_ms).unwrap(), Outcome::OutsideWindow);
    assert!(host.systemctl_log().is_empty());
    // An hour later it is inside the window.
    assert!(matches!(
        run(&host.config, until_ms + 3_600_000).unwrap(),
        Outcome::Updated { .. }
    ));
}

#[test]
fn now_bypasses_apply_after_and_the_window() {
    let mut host = Host::new("0");
    host.publish("0.1.4", 2, NOW + 86_400_000);
    host.config.window = Some(Window::parse("00:00-00:01").unwrap());
    let midday = NOW - NOW % 86_400_000 + 43_200_000;
    assert!(matches!(
        run(&host.config, midday).unwrap(),
        Outcome::Waiting { .. }
    ));
    host.config.now = true;
    assert!(matches!(
        run(&host.config, midday).unwrap(),
        Outcome::Updated { .. }
    ));
}

#[test]
fn disk_shortfall_is_loud_and_leaves_the_binary_alone() {
    let mut host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    let size = fake_binary("0.1.4").len() as u64;
    host.config.available_bytes = |_| Ok(10);
    let error = run(&host.config, NOW).unwrap_err();
    assert!(
        matches!(
            error,
            UpdateError::Disk { needed_bytes, available_bytes: 10 } if needed_bytes == size * 3
        ),
        "{error}"
    );
    let status = host.json("state/update-status.json").unwrap();
    assert_eq!(status["blocked"], "disk");
    assert_eq!(status["needed_bytes"], size * 3);
    assert_eq!(status["available_bytes"], 10);
    host.assert_installed("0.1.3");
    assert!(!host.path("bin/.sn46-validator.new").exists());
    assert!(host.systemctl_log().is_empty());
    // With room again the status clears and the update goes ahead.
    host.config.available_bytes = sn46_validator::update::available_bytes;
    assert!(matches!(
        run(&host.config, NOW).unwrap(),
        Outcome::Updated { .. }
    ));
    assert!(!host.path("state/update-status.json").exists());
}

#[test]
fn a_binary_that_does_not_match_is_never_installed() {
    let host = Host::new("0");
    // Corrupted in transit: same size, other bytes. Retried next time.
    host.publish("0.1.4", 2, 0);
    let mut corrupt = fake_binary("0.1.4");
    let last = corrupt.len() - 2;
    corrupt[last] = b'X';
    fs::write(host.path("release").join(BINARY_NAME), &corrupt).unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::Binary(_)
    ));
    assert!(host.json("state/update-failed.json").is_none());
    // Signed and intact, but it reports another version: never retried.
    host.publish_binary("0.1.4", 3, 0, &fake_binary("0.1.5"));
    let error = run(&host.config, NOW).unwrap_err();
    assert!(matches!(error, UpdateError::BadRelease(_)), "{error}");
    assert!(error.to_string().contains("--version"), "{error}");
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::PreviouslyFailed);
    host.assert_installed("0.1.3");
    assert!(!host.path("bin/.sn46-validator.new").exists());
    assert!(host.systemctl_log().is_empty());
}

#[test]
fn a_crash_looping_release_is_rolled_back_and_not_retried() {
    let mut host = Host::new("n");
    host.config.health = Duration::from_millis(100);
    host.publish("0.1.4", 2, 0);
    let error = run(&host.config, NOW).unwrap_err();
    assert!(
        matches!(&error, UpdateError::RolledBack { version, .. } if version == "0.1.4"),
        "{error}"
    );
    host.assert_installed("0.1.3");
    assert!(!host.path("bin/sn46-validator.previous").exists());
    assert_eq!(host.installed(), None);
    let failed = host.json("state/update-failed.json").unwrap();
    assert_eq!(failed["failed"].as_array().unwrap().len(), 1);
    assert_eq!(failed["failed"][0]["sequence"], 2);
    assert_eq!(failed["failed"][0]["version"], "0.1.4");
    let log = host.systemctl_log();
    assert_eq!(log.matches("restart sn46-validator").count(), 2, "{log}");
    // The failed sequence is suppressed; a newer release is tried again.
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::PreviouslyFailed);
    host.publish("0.1.5", 3, 0);
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    host.assert_installed("0.1.3");
}

#[test]
fn a_service_that_is_not_active_is_rolled_back() {
    let mut host = Host::new("0");
    script(
        &host.path("systemctl"),
        &format!(
            "echo \"$*\" >> {}\n[ \"$1\" = show ] && printf 'ActiveState=activating\\nNRestarts=0\\n'\nexit 0\n",
            host.path("systemctl.log").display()
        ),
    );
    host.config.health = Duration::from_secs(1);
    host.publish("0.1.4", 2, 0);
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    host.assert_installed("0.1.3");
}

/// The state a run killed between the swap and the health check leaves behind, as seen by
/// the next run, which is the new binary.
fn interrupted_after_the_swap(host: &mut Host) {
    host.publish("0.1.4", 2, 0);
    fs::rename(
        host.path("bin/sn46-validator"),
        host.path("bin/sn46-validator.previous"),
    )
    .unwrap();
    fs::write(host.path("bin/sn46-validator"), fake_binary("0.1.4")).unwrap();
    fs::write(
        host.path("state/pending.json"),
        r#"{"sequence":2,"version":"0.1.4"}"#,
    )
    .unwrap();
    host.config.running_version = "0.1.4".into();
}

#[test]
fn an_interrupted_update_is_health_checked_before_it_is_recorded() {
    let mut host = Host::new("0");
    interrupted_after_the_swap(&mut host);
    assert_eq!(
        run(&host.config, NOW).unwrap(),
        Outcome::Updated {
            version: "0.1.4".into()
        }
    );
    assert!(host.systemctl_log().starts_with("restart sn46-validator\n"));
    assert_eq!(
        host.installed(),
        Some(json!({"sequence": 2, "version": "0.1.4"}))
    );
    assert!(!host.path("state/pending.json").exists());
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::NotNewer);
}

#[test]
fn an_interrupted_unhealthy_update_is_rolled_back() {
    let mut host = Host::new("n");
    host.config.health = Duration::from_millis(100);
    interrupted_after_the_swap(&mut host);
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    host.assert_installed("0.1.3");
    assert_eq!(host.installed(), None);
    assert!(!host.path("state/pending.json").exists());
    assert_eq!(
        host.json("state/update-failed.json").unwrap()["failed"][0]["sequence"],
        2
    );
}

#[test]
fn a_failure_that_cannot_be_recorded_still_rolls_back() {
    let mut host = Host::new("n");
    host.config.health = Duration::from_millis(100);
    host.publish("0.1.4", 2, 0);
    // Writing the failure record now fails.
    fs::create_dir(host.path("state/update-failed.json.tmp")).unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RollbackIncomplete { .. }
    ));
    host.assert_installed("0.1.3");
    assert_eq!(host.systemctl_log().matches("restart").count(), 2);
    // The marker is the only evidence 0.1.4 failed: it stays until the failure is recorded,
    // and the release is not installed again meanwhile.
    assert_eq!(
        host.json("state/pending.json").unwrap()["rolling_back"],
        true
    );
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RollbackIncomplete { .. }
    ));
    host.assert_installed("0.1.3");
    fs::remove_dir(host.path("state/update-failed.json.tmp")).unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    assert!(!host.path("state/pending.json").exists());
    assert_eq!(
        host.json("state/update-failed.json").unwrap()["failed"][0]["sequence"],
        2
    );
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::PreviouslyFailed);
    host.assert_installed("0.1.3");
}

#[test]
fn a_rollback_whose_restart_fails_is_finished_by_the_next_run() {
    let mut host = Host::new("n");
    host.config.health = Duration::from_millis(100);
    // Unhealthy after the first restart; every later restart fails while `fail` exists.
    script(
        &host.path("systemctl"),
        &format!(
            "log={log}\necho \"$*\" >> \"$log\"\n             case \"$1\" in\n  restart) [ \"$(grep -c '^restart' \"$log\")\" -ge 2 ] && [ -e {fail} ] && exit 1; exit 0;;\n               show) n=$(grep -c '^show' \"$log\"); echo ActiveState=active; echo NRestarts=$n;;\nesac\n",
            log = host.path("systemctl.log").display(),
            fail = host.path("fail").display(),
        ),
    );
    fs::write(host.path("fail"), "").unwrap();
    host.publish("0.1.4", 2, 0);
    let error = run(&host.config, NOW).unwrap_err();
    assert!(
        matches!(error, UpdateError::RollbackIncomplete { .. }),
        "{error}"
    );
    host.assert_installed("0.1.3");
    assert_eq!(
        host.json("state/pending.json").unwrap()["rolling_back"],
        true
    );
    // Still failing: the marker survives another attempt.
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RollbackIncomplete { .. }
    ));
    fs::remove_file(host.path("fail")).unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    assert!(!host.path("state/pending.json").exists());
    host.assert_installed("0.1.3");
    let failed = host.json("state/update-failed.json").unwrap();
    assert_eq!(failed["failed"].as_array().unwrap().len(), 1);
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::PreviouslyFailed);
}

#[test]
fn a_rollback_interrupted_after_the_restore_restarts_the_service() {
    let host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    fs::write(
        host.path("state/pending.json"),
        r#"{"rolling_back":true,"sequence":2,"version":"0.1.4"}"#,
    )
    .unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    assert_eq!(host.systemctl_log(), "restart sn46-validator\n");
    host.assert_installed("0.1.3");
    assert!(!host.path("state/pending.json").exists());
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::PreviouslyFailed);
}

#[test]
fn a_rollback_that_could_not_write_any_state_is_still_recognised() {
    let host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    // The rollback restored 0.1.3 (no `.previous` left) but could neither mark
    // `pending.json` nor record the failure.
    fs::write(
        host.path("state/pending.json"),
        r#"{"rolling_back":false,"sequence":2,"version":"0.1.4"}"#,
    )
    .unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    host.assert_installed("0.1.3");
    assert!(!host.path("state/pending.json").exists());
    assert_eq!(
        host.json("state/update-failed.json").unwrap()["failed"][0]["sequence"],
        2
    );
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::PreviouslyFailed);
    host.assert_installed("0.1.3");
}

#[test]
fn a_state_dir_that_turns_read_only_after_the_swap_loses_no_failure() {
    let mut host = Host::new("n");
    host.config.health = Duration::from_millis(100);
    // The first restart (of the new release) makes the state directory read-only.
    script(
        &host.path("systemctl"),
        &format!(
            "log={log}\necho \"$*\" >> \"$log\"\n\
             case \"$1\" in\n  restart) [ \"$(grep -c '^restart' \"$log\")\" = 1 ] && chmod 0500 {state}; exit 0;;\n  \
             show) n=$(grep -c '^show' \"$log\"); echo ActiveState=active; echo NRestarts=$n;;\nesac\n",
            log = host.path("systemctl.log").display(),
            state = host.path("state").display(),
        ),
    );
    host.publish("0.1.4", 2, 0);
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RollbackIncomplete { .. }
    ));
    host.assert_installed("0.1.3");
    assert_eq!(
        host.json("state/pending.json").unwrap()["rolling_back"],
        false
    );
    fs::set_permissions(host.path("state"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::RolledBack { .. }
    ));
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::PreviouslyFailed);
    host.assert_installed("0.1.3");
}

#[test]
fn a_pending_update_that_never_landed_is_dropped() {
    let host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    fs::copy(
        host.path("bin/sn46-validator"),
        host.path("bin/sn46-validator.previous"),
    )
    .unwrap();
    fs::write(
        host.path("state/pending.json"),
        r#"{"sequence":2,"version":"0.1.4"}"#,
    )
    .unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap(),
        Outcome::Updated { .. }
    ));
    assert!(!host.path("state/pending.json").exists());
}

#[test]
fn a_state_dir_others_can_write_is_refused() {
    let host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    fs::set_permissions(host.path("state"), fs::Permissions::from_mode(0o775)).unwrap();
    assert!(matches!(
        run(&host.config, NOW).unwrap_err(),
        UpdateError::Config(_)
    ));
    host.assert_installed("0.1.3");
}

#[test]
fn state_writes_do_not_follow_symlinks() {
    let host = Host::new("0");
    host.publish("0.1.3", 7, 0);
    fs::write(host.path("target"), "keep").unwrap();
    std::os::unix::fs::symlink(host.path("target"), host.path("state/release.json.tmp")).unwrap();
    assert!(run(&host.config, NOW).is_err());
    assert_eq!(fs::read_to_string(host.path("target")).unwrap(), "keep");
}

#[test]
fn a_held_lock_skips_the_run() {
    let host = Host::new("0");
    host.publish("0.1.4", 2, 0);
    let lock = fs::File::create(host.path("state/update.lock")).unwrap();
    lock.lock().unwrap();
    assert_eq!(run(&host.config, NOW).unwrap(), Outcome::Busy);
    host.assert_installed("0.1.3");
}

fn update_binary(state: &Path, envs: &[(&str, String)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sn46-validator"))
        .arg("update")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("NO_COLOR", "1")
        .env("UPDATE_STATE_DIR", state)
        .env("UPDATE_INSTALL_PATH", state.join("bin/sn46-validator"))
        .envs(envs.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap()
}

#[test]
fn auto_update_zero_exits_cleanly_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let output = update_binary(dir.path(), &[("AUTO_UPDATE", "0".into())]);
    assert!(output.status.success());
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("Auto-update is disabled"), "{log}");
}

#[test]
fn the_binary_refuses_a_manifest_signed_with_another_key() {
    let dir = tempfile::tempdir().unwrap();
    let raw = manifest("9.9.9", 1, 0, &fake_binary("9.9.9"));
    let signature = SigningKey::from_bytes(&[9; 32]).sign(&raw);
    fs::write(dir.path().join("manifest.json"), &raw).unwrap();
    fs::write(
        dir.path().join("manifest.sig"),
        hex::encode(signature.to_bytes()),
    )
    .unwrap();
    let url = format!("file://{}/manifest.json", dir.path().display());
    let output = update_binary(dir.path(), &[("UPDATE_MANIFEST_URL", url)]);
    assert!(!output.status.success());
    let log = String::from_utf8_lossy(&output.stderr);
    assert!(log.contains("signature does not verify"), "{log}");
    assert!(!dir.path().join("release.json").exists());
}

#[test]
fn now_is_only_for_update() {
    let output = Command::new(env!("CARGO_BIN_EXE_sn46-validator"))
        .args(["run-once", "--now"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
