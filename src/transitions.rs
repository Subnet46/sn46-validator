//! The incentive record the validator last scored a summary under (`transitions.json`).
//!
//! A v4 summary is scored under the schedule record in force at its epoch. The validator keeps
//! that record (`head`), the one before it (`previous`: the head of the summary the state held
//! when it was written) and the summary it belongs to, beside the state file. It is written
//! before `state.json` commits the same summary, so after a crash between the two the head is
//! newer than the state and rolls back to `previous`. A state that a rolled-back older binary
//! advanced (it writes no transitions) has its head derived again from the verified history at
//! the state's epoch. While a head is held, a later summary must be scored under that record or
//! one after it in a history that still holds it: a platform history that was rewritten or lost
//! records under the validator's feet is refused, not followed.
//!
//! The state file's own format is unchanged.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sn46_shared::schedule::{History, Kind, Record};

use crate::state::{StateError, StateStore, ValidatorState, save_bytes};

/// A schedule record as the validator accepted it: its id and its signed digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRef {
    pub digest: String,
    pub id: String,
}

impl From<&Record> for RecordRef {
    fn from(record: &Record) -> Self {
        Self {
            digest: record.digest.clone(),
            id: record.id.clone(),
        }
    }
}

/// `transitions.json`. Fields are in key order, so serialising it yields the canonical bytes.
/// `summary_epoch_end_block` places the summary against the state's, which tells a crash
/// between the two writes (transitions ahead) from an older binary's run (state ahead).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transitions {
    pub head: Option<RecordRef>,
    pub previous: Option<RecordRef>,
    pub summary_digest: String,
    pub summary_epoch_end_block: u64,
    pub summary_id: String,
}

/// Where the head stands for the summary the state holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// The head of the state's summary: `None` before any record was scored under.
    Known(Option<RecordRef>),
    /// The state is not the one the transitions were written for (an older binary advanced
    /// it, or there are none yet): the head is the record in force at the state's epoch.
    Derive,
}

impl Head {
    /// The head, deriving it from `history` at `epoch` (the state's summary's) when needed.
    pub fn resolve(self, history: &History, epoch: Option<u64>) -> Option<RecordRef> {
        match self {
            Self::Known(head) => head,
            Self::Derive => epoch
                .and_then(|epoch| history.in_force(Kind::Incentive, epoch))
                .map(|(record, _)| RecordRef::from(record)),
        }
    }
}

/// Why a summary's record does not continue the head.
#[derive(Debug, thiserror::Error)]
pub enum HeadError {
    #[error(
        "the schedule history no longer holds {0}, the record this validator last scored under"
    )]
    Missing(String),
    #[error(
        "the summary is scored under {record}, a record before {head}, the one this validator last scored under"
    )]
    Behind { record: String, head: String },
    #[error("no record is in force for the summary, but this validator last scored under {0}")]
    Dropped(String),
}

/// `in_force` (the record a v4 summary is scored under, `None` for the genesis) continues
/// `head` in `history`: the history still holds the head, by id and digest, and the record is
/// it or one after it.
pub fn check_head(
    head: Option<&RecordRef>,
    in_force: Option<&Record>,
    history: &History,
) -> Result<(), HeadError> {
    let Some(head) = head else {
        return Ok(());
    };
    let held = history
        .records()
        .iter()
        .find(|record| record.id == head.id && record.digest == head.digest)
        .ok_or_else(|| HeadError::Missing(head.id.clone()))?;
    let Some(record) = in_force else {
        return Err(HeadError::Dropped(head.id.clone()));
    };
    if record.revision < held.revision {
        return Err(HeadError::Behind {
            record: record.id.clone(),
            head: head.id.clone(),
        });
    }
    Ok(())
}

/// The transitions file beside a state file.
pub struct TransitionsStore {
    pub path: PathBuf,
}

impl TransitionsStore {
    pub fn beside(state: &StateStore) -> Self {
        Self {
            path: state.path.with_extension("transitions.json"),
        }
    }

    pub fn load(&self) -> Result<Option<Transitions>, StateError> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StateError::ReadTransitions(source)),
        };
        serde_json::from_slice(&raw)
            .map(Some)
            .map_err(StateError::MalformedTransitions)
    }

    pub fn save(&self, transitions: &Transitions) -> Result<(), StateError> {
        let mut data = serde_json::to_vec(transitions).map_err(StateError::MalformedTransitions)?;
        data.push(b'\n');
        save_bytes(&self.path, &data)
    }

    /// The head for the summary `state` holds. Transitions written for a summary the state
    /// does not hold yet (a crash before the state committed it) roll back to `previous`, and
    /// the rollback is saved; transitions behind the state (an older binary ran) leave the head
    /// to be derived.
    pub fn reconcile(&self, state: &ValidatorState) -> Result<Head, StateError> {
        let Some(transitions) = self.load()? else {
            return Ok(if state.summary_id.is_none() {
                Head::Known(None)
            } else {
                Head::Derive
            });
        };
        let held = (
            state.summary_id.as_deref(),
            state.summary_digest.as_deref(),
            state.summary_epoch_end_block,
        );
        if held
            == (
                Some(transitions.summary_id.as_str()),
                Some(transitions.summary_digest.as_str()),
                Some(transitions.summary_epoch_end_block),
            )
        {
            return Ok(Head::Known(transitions.head));
        }
        let ahead = state
            .summary_epoch_end_block
            .is_none_or(|end| transitions.summary_epoch_end_block > end);
        if !ahead {
            tracing::warn!(
                transitions = transitions.summary_id,
                state = state.summary_id.as_deref().unwrap_or(""),
                "Transitions are behind the state (an older release ran): the head is derived from the schedule history"
            );
            return Ok(Head::Derive);
        }
        tracing::warn!(
            transitions = transitions.summary_id,
            state = state.summary_id.as_deref().unwrap_or(""),
            "Transitions are ahead of the state (a run stopped between the two writes): the head rolls back"
        );
        let head = transitions.previous;
        match (
            &state.summary_id,
            &state.summary_digest,
            state.summary_epoch_end_block,
        ) {
            (Some(id), Some(digest), Some(end)) => self.save(&Transitions {
                head: head.clone(),
                previous: None,
                summary_digest: digest.clone(),
                summary_epoch_end_block: end,
                summary_id: id.clone(),
            })?,
            _ => match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(source) => return Err(StateError::Save(source)),
            },
        }
        Ok(Head::Known(head))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(id: &str) -> RecordRef {
        RecordRef {
            digest: format!("sha256:{id}"),
            id: id.into(),
        }
    }

    fn state(id: &str, end: u64) -> ValidatorState {
        ValidatorState {
            summary_id: Some(id.into()),
            summary_digest: Some(format!("sha256:{id}")),
            summary_epoch_end_block: Some(end),
            burn_epoch_end_block: None,
        }
    }

    fn written(head: Option<&str>, previous: Option<&str>, id: &str, end: u64) -> Transitions {
        Transitions {
            head: head.map(reference),
            previous: previous.map(reference),
            summary_digest: format!("sha256:{id}"),
            summary_epoch_end_block: end,
            summary_id: id.into(),
        }
    }

    fn stores() -> (tempfile::TempDir, StateStore, TransitionsStore) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateStore::new(dir.path().join("state.json"));
        let transitions = TransitionsStore::beside(&state);
        (dir, state, transitions)
    }

    #[test]
    fn the_file_sits_beside_the_state_in_key_order() {
        let (dir, _, store) = stores();
        assert_eq!(store.path, dir.path().join("state.transitions.json"));
        store
            .save(&written(
                Some("incentive-2"),
                Some("incentive-1"),
                "s2",
                7920,
            ))
            .unwrap();
        assert_eq!(
            fs::read_to_string(&store.path).unwrap(),
            concat!(
                r#"{"head":{"digest":"sha256:incentive-2","id":"incentive-2"},"#,
                r#""previous":{"digest":"sha256:incentive-1","id":"incentive-1"},"#,
                r#""summary_digest":"sha256:s2","summary_epoch_end_block":7920,"summary_id":"s2"}"#,
                "\n"
            )
        );
        fs::write(&store.path, b"{}").unwrap();
        assert!(matches!(
            store.load(),
            Err(StateError::MalformedTransitions(_))
        ));
    }

    #[test]
    fn transitions_for_the_state_hold_their_head() {
        let (_dir, _, store) = stores();
        assert_eq!(
            store.reconcile(&ValidatorState::default()).unwrap(),
            Head::Known(None)
        );
        // A state and no transitions: an older release wrote it.
        assert_eq!(store.reconcile(&state("s1", 720)).unwrap(), Head::Derive);
        store
            .save(&written(Some("incentive-1"), None, "s1", 720))
            .unwrap();
        assert_eq!(
            store.reconcile(&state("s1", 720)).unwrap(),
            Head::Known(Some(reference("incentive-1")))
        );
    }

    /// Killed after transitions were written for s2 and before the state committed it: the
    /// head rolls back to the one of s1, and that is saved.
    #[test]
    fn a_head_ahead_of_the_state_rolls_back_to_previous() {
        let (_dir, _, store) = stores();
        store
            .save(&written(
                Some("incentive-2"),
                Some("incentive-1"),
                "s2",
                7920,
            ))
            .unwrap();
        assert_eq!(
            store.reconcile(&state("s1", 720)).unwrap(),
            Head::Known(Some(reference("incentive-1")))
        );
        assert_eq!(
            store.load().unwrap().unwrap(),
            written(Some("incentive-1"), None, "s1", 720)
        );
        // The first summary never committed: no head, and no file.
        store
            .save(&written(Some("incentive-1"), None, "s1", 720))
            .unwrap();
        assert_eq!(
            store.reconcile(&ValidatorState::default()).unwrap(),
            Head::Known(None)
        );
        assert!(store.load().unwrap().is_none());
    }

    /// An older binary advanced the state past the transitions: the head is derived from the
    /// history at the state's epoch.
    #[test]
    fn a_state_ahead_of_the_transitions_derives_the_head() {
        use crate::schedule::tests::{key, two_records};
        let (_dir, _, store) = stores();
        store
            .save(&written(Some("incentive-1"), None, "s1", 720))
            .unwrap();
        let head = store.reconcile(&state("s22", 7920)).unwrap();
        assert_eq!(head, Head::Derive);
        let dave = key("//Dave");
        let mut history = History::default();
        crate::schedule::extend(
            &mut history,
            two_records(&dave, &dave, "local"),
            &crate::schedule::Trust {
                network: "local",
                netuid: 46,
                signers: &[crate::schedule::tests::ss58(&dave)],
            },
        )
        .unwrap();
        let derived = head.clone().resolve(&history, Some(22)).unwrap();
        assert_eq!(derived.id, "incentive-2");
        assert_eq!(
            head.clone().resolve(&history, Some(5)).unwrap().id,
            "incentive-1"
        );
        assert_eq!(head.resolve(&history, None), None);
        // The same epoch under another summary is no rollback either.
        assert_eq!(store.reconcile(&state("other", 720)).unwrap(), Head::Derive);
    }

    #[test]
    fn a_record_must_continue_the_head() {
        use crate::schedule::tests::{key, two_records};
        let dave = key("//Dave");
        let records = two_records(&dave, &dave, "local");
        let mut history = History::default();
        crate::schedule::extend(
            &mut history,
            records.clone(),
            &crate::schedule::Trust {
                network: "local",
                netuid: 46,
                signers: &[crate::schedule::tests::ss58(&dave)],
            },
        )
        .unwrap();
        let first = RecordRef::from(&records[0]);
        let second = RecordRef::from(&records[1]);
        check_head(None, None, &history).unwrap();
        check_head(Some(&first), Some(&records[1]), &history).unwrap();
        check_head(Some(&second), Some(&records[1]), &history).unwrap();
        assert!(matches!(
            check_head(Some(&second), Some(&records[0]), &history),
            Err(HeadError::Behind { .. })
        ));
        assert!(matches!(
            check_head(Some(&first), None, &history),
            Err(HeadError::Dropped(_))
        ));
        let forged = RecordRef {
            digest: "sha256:other".into(),
            ..second
        };
        assert!(matches!(
            check_head(Some(&forged), Some(&records[1]), &history),
            Err(HeadError::Missing(id)) if id == "incentive-2"
        ));
    }
}
