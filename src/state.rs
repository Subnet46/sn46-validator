//! Small durable state for one Validator.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical::canonical_json;

/// The local Validator state is invalid or cannot be saved.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("Cannot open validator state lock: {0}")]
    OpenLock(#[source] std::io::Error),
    #[error("Cannot acquire validator state lock: already locked")]
    LockContended,
    #[error("Cannot acquire validator state lock: {0}")]
    AcquireLock(#[source] std::io::Error),
    #[error("Validator state is unreadable")]
    Read(#[source] std::io::Error),
    #[error("Validator state is unreadable")]
    Malformed(#[source] serde_json::Error),
    #[error("Validator state fields are invalid")]
    InvalidFields(#[source] serde_json::Error),
    #[error("Validator summary state is incomplete")]
    IncompleteEpochSummary,
    #[error("Validator summary ID is invalid")]
    InvalidEpochSummaryId(#[source] Option<serde_json::Error>),
    #[error("Validator summary digest is invalid")]
    InvalidEpochSummaryDigest(#[source] Option<serde_json::Error>),
    #[error("Validator summary epoch is invalid")]
    InvalidEpochSummaryEpoch(#[source] serde_json::Error),
    #[error("Validator burn epoch is invalid")]
    InvalidBurnEpoch(#[source] serde_json::Error),
    #[error("Validator burn epoch is inconsistent")]
    InconsistentBurnEpoch,
    #[error("Platform returned a summary older than local state")]
    OlderEpochSummary,
    #[error("A processed summary changed")]
    ChangedEpochSummary,
    #[error("Validator state could not be saved")]
    Save(#[source] std::io::Error),
}

/// Fields are in key order, so serialising it yields the canonical state bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ValidatorState {
    pub burn_epoch_end_block: Option<u64>,
    pub summary_digest: Option<String>,
    pub summary_epoch_end_block: Option<u64>,
    pub summary_id: Option<String>,
}

/// The state file's shape: exactly these four keys, values checked one by one below so each
/// failure can name its own field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    burn_epoch_end_block: Value,
    summary_digest: Value,
    summary_epoch_end_block: Value,
    summary_id: Value,
}

/// Compact JSON with every non-ASCII character spelled as `\uXXXX` UTF-16 units;
/// serde_json already escapes the control characters.
fn ascii_json<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out = String::new();
    for character in canonical_json(value).chars() {
        if character.is_ascii() {
            out.push(character);
        } else {
            let mut units = [0u16; 2];
            for unit in character.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

pub struct StateStore {
    pub path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Hold this file for the entire read/score/burn/save transaction. A separate inode
    /// keeps the lock valid when the state file is atomically replaced.
    pub fn lock(&self) -> Result<File, StateError> {
        let open = || -> std::io::Result<File> {
            create_directory(directory_of(&self.path))?;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(self.path.with_added_extension("lock"))
        };
        let lock = open().map_err(StateError::OpenLock)?;
        lock.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => StateError::LockContended,
            fs::TryLockError::Error(source) => StateError::AcquireLock(source),
        })?;
        Ok(lock)
    }

    pub fn load(&self) -> Result<ValidatorState, StateError> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                return Ok(ValidatorState::default());
            }
            Err(source) => return Err(StateError::Read(source)),
        };
        // Duplicate keys collapse before the struct sees them: the last one wins.
        let value: Value = serde_json::from_slice(&raw).map_err(StateError::Malformed)?;
        let file: StateFile = serde_json::from_value(value).map_err(StateError::InvalidFields)?;
        if file.summary_id.is_null() != file.summary_digest.is_null()
            || file.summary_id.is_null() != file.summary_epoch_end_block.is_null()
        {
            return Err(StateError::IncompleteEpochSummary);
        }
        let text = |value: Value, invalid: fn(Option<serde_json::Error>) -> StateError| {
            match serde_json::from_value::<Option<String>>(value) {
                Ok(Some(text)) if text.is_empty() => Err(invalid(None)),
                Ok(text) => Ok(text),
                Err(source) => Err(invalid(Some(source))),
            }
        };
        let state = ValidatorState {
            summary_id: text(file.summary_id, StateError::InvalidEpochSummaryId)?,
            summary_digest: text(file.summary_digest, StateError::InvalidEpochSummaryDigest)?,
            summary_epoch_end_block: serde_json::from_value(file.summary_epoch_end_block)
                .map_err(StateError::InvalidEpochSummaryEpoch)?,
            burn_epoch_end_block: serde_json::from_value(file.burn_epoch_end_block)
                .map_err(StateError::InvalidBurnEpoch)?,
        };
        if let Some(burn) = state.burn_epoch_end_block
            && state
                .summary_epoch_end_block
                .is_none_or(|epoch_summary| burn > epoch_summary)
        {
            return Err(StateError::InconsistentBurnEpoch);
        }
        Ok(state)
    }

    /// The exact bytes `save` writes: keys sorted, compact separators, ASCII escapes,
    /// trailing newline.
    pub fn encode(state: &ValidatorState) -> Vec<u8> {
        let mut data = ascii_json(state).into_bytes();
        data.push(b'\n');
        data
    }

    pub fn save(&self, state: &ValidatorState) -> Result<(), StateError> {
        save_bytes(&self.path, &Self::encode(state))
    }
}

pub(crate) fn save_bytes(path: &Path, data: &[u8]) -> Result<(), StateError> {
    // run_once holds the state lock while writing either state or its submission guard.
    let temporary = path.with_added_extension("tmp");
    let result = write_durably(directory_of(path), &temporary, path, data);
    if temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(StateError::Save)
}

/// `Path::parent` of a bare filename is `""`, which no filesystem call accepts.
fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn write_durably(
    parent: &Path,
    temporary: &Path,
    target: &Path,
    data: &[u8],
) -> std::io::Result<()> {
    create_directory(parent)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(temporary)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, target)?;
    File::open(parent)?.sync_all()
}

fn create_directory(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        (dir, StateStore::new(path))
    }

    fn full() -> ValidatorState {
        ValidatorState {
            summary_id: Some("summary-1".into()),
            summary_digest: Some("sha256:digest".into()),
            summary_epoch_end_block: Some(720),
            burn_epoch_end_block: Some(720),
        }
    }

    #[test]
    fn missing_state_is_empty_and_saved_state_is_durable() {
        let (_dir, store) = store();
        assert_eq!(store.load().unwrap(), ValidatorState::default());
        store.save(&full()).unwrap();
        assert_eq!(StateStore::new(store.path.clone()).load().unwrap(), full());
        let mode = fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let under_a_file = StateStore::new(store.path.join("state.json"));
        assert_eq!(under_a_file.load().unwrap(), ValidatorState::default());
        assert!(!store.path.with_added_extension("tmp").exists());
    }

    #[test]
    fn lock_survives_state_replacement_and_releases_on_drop() {
        let (_dir, store) = store();
        let lock = store.lock().unwrap();
        store.save(&full()).unwrap();
        assert!(matches!(
            StateStore::new(store.path.clone()).lock(),
            Err(StateError::LockContended)
        ));
        let other = StateStore::new(store.path.with_file_name("other.json"));
        assert!(other.lock().is_ok());
        drop(lock);
        assert!(store.lock().is_ok());
    }

    #[test]
    fn malformed_or_partial_state_fails_closed() {
        let (_dir, store) = store();
        fs::write(&store.path, b"not-json").unwrap();
        let error = store.load().unwrap_err();
        assert!(matches!(error, StateError::Malformed(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        for raw in [&b"{}"[..], br#"{"summary_id":"only"}"#] {
            fs::write(&store.path, raw).unwrap();
            let error = store.load().unwrap_err();
            assert!(matches!(error, StateError::InvalidFields(_)));
            assert!(error.source().unwrap().is::<serde_json::Error>());
        }
    }

    #[test]
    fn invalid_fields_have_distinct_variants_and_decode_sources() {
        let (_dir, store) = store();
        let invalid = |field: &str, value: Value| {
            let mut document = serde_json::to_value(full()).unwrap();
            document[field] = value;
            fs::write(&store.path, serde_json::to_vec(&document).unwrap()).unwrap();
            store.load().unwrap_err()
        };
        assert!(matches!(
            invalid("summary_id", Value::Null),
            StateError::IncompleteEpochSummary
        ));
        assert!(matches!(
            invalid("summary_id", "".into()),
            StateError::InvalidEpochSummaryId(None)
        ));
        let error = invalid("summary_id", 1.into());
        assert!(matches!(error, StateError::InvalidEpochSummaryId(Some(_))));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(matches!(
            invalid("summary_digest", "".into()),
            StateError::InvalidEpochSummaryDigest(None)
        ));
        let error = invalid("summary_digest", true.into());
        assert!(matches!(
            error,
            StateError::InvalidEpochSummaryDigest(Some(_))
        ));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        let error = invalid("summary_epoch_end_block", (-1).into());
        assert!(matches!(error, StateError::InvalidEpochSummaryEpoch(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        let error = invalid("burn_epoch_end_block", (-1).into());
        assert!(matches!(error, StateError::InvalidBurnEpoch(_)));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(matches!(
            invalid("burn_epoch_end_block", 721.into()),
            StateError::InconsistentBurnEpoch
        ));
    }

    #[test]
    fn inaccessible_state_fails_closed() {
        let (dir, store) = store();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o000)).unwrap();
        let result = store.load();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.unwrap_err();
        assert!(
            matches!(&error, StateError::Read(source) if source.kind() == ErrorKind::PermissionDenied)
        );
        assert!(error.source().unwrap().is::<std::io::Error>());
        assert_eq!(error.to_string(), "Validator state is unreadable");
    }

    #[test]
    fn bare_filename_saves_into_the_working_directory() {
        assert_eq!(directory_of(Path::new("state.json")), Path::new("."));
        assert_eq!(directory_of(Path::new("run/state.json")), Path::new("run"));
    }

    #[test]
    fn save_rejects_an_unwritable_directory() {
        let store = StateStore::new("/proc/sn46-validator/state.json");
        let error = store.save(&ValidatorState::default()).unwrap_err();
        assert!(matches!(error, StateError::Save(_)));
        assert!(error.source().unwrap().is::<std::io::Error>());
        assert_eq!(error.to_string(), "Validator state could not be saved");
        let error = store.lock().unwrap_err();
        assert!(matches!(error, StateError::OpenLock(_)));
        assert!(error.source().unwrap().is::<std::io::Error>());
    }

    #[test]
    fn incompatible_state_is_refused_without_rewriting_it() {
        let (_dir, store) = store();
        let raw = br#"{"obsolete_id":"local-46-361-720","burn_epoch_end_block":720}"#;
        fs::write(&store.path, raw).unwrap();
        assert!(matches!(store.load(), Err(StateError::InvalidFields(_))));
        assert_eq!(fs::read(&store.path).unwrap(), raw);
    }

    fn golden() -> Value {
        serde_json::from_str(
            &fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/golden/state_cases.json"
            ))
            .unwrap(),
        )
        .unwrap()
    }

    fn state_of(value: &Value) -> ValidatorState {
        ValidatorState {
            summary_id: value["summary_id"].as_str().map(str::to_owned),
            summary_digest: value["summary_digest"].as_str().map(str::to_owned),
            summary_epoch_end_block: value["summary_epoch_end_block"].as_u64(),
            burn_epoch_end_block: value["burn_epoch_end_block"].as_u64(),
        }
    }

    #[test]
    fn golden_load_verdicts() {
        let (_dir, store) = store();
        for case in golden()["load"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let raw = match case.get("raw_hex") {
                Some(hex_text) => hex::decode(hex_text.as_str().unwrap()).unwrap(),
                None => case["raw"].as_str().unwrap().as_bytes().to_vec(),
            };
            fs::write(&store.path, raw).unwrap();
            let result = store.load();
            if name == "epoch_2_64" {
                // Block numbers are u64; the golden 2^64 case is rejected here.
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "Validator summary epoch is invalid"
                );
                continue;
            }
            match case["verdict"].as_str().unwrap() {
                "ok" => assert_eq!(result.unwrap(), state_of(&case["state"]), "{name}"),
                _ => assert_eq!(result.unwrap_err().to_string(), case["message"], "{name}"),
            }
        }
    }

    #[test]
    fn golden_save_bytes_round_trip() {
        let (_dir, store) = store();
        for case in golden()["save"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let state = state_of(&case["state"]);
            let expected = case["bytes"].as_str().unwrap().as_bytes();
            assert_eq!(StateStore::encode(&state), expected, "{name}");
            assert_eq!(case["mode"], "0o600");
            store.save(&state).unwrap();
            assert_eq!(fs::read(&store.path).unwrap(), expected, "{name}");
            fs::write(&store.path, expected).unwrap();
            assert_eq!(store.load().unwrap(), state, "{name}");
        }
    }
}
