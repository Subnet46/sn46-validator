//! The platform's signed schedule history, as this validator has verified it.
//!
//! Every record is authenticated by one of the trusted platform keys (`PLATFORM_SIGNER`, more
//! than one while a key rotates: records stay signed by the key of their time), bound to the
//! validator's network and netuid, and admitted in turn by the shared validation table. What
//! was verified is kept beside the state file, so each run fetches only the records after the
//! last one it holds (`?since=<revision>`); the kept copy is replayed again on load, and a
//! copy that no longer replays, or that the platform's new records do not continue, is
//! dropped for a full fetch.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use sn46_shared::schedule::{Error, History, Kind, Reason, Record};

use crate::runtime::{Resource, Run, RunError};
use crate::state::{StateStore, save_bytes};

/// Why a history is not trusted.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("{0}")]
    Record(#[from] Error),
    #[error("record {id}: signed by {signer}, which is not a trusted platform key")]
    Signer { id: String, signer: String },
    #[error("record {id}: for {network}/{netuid}, not this validator's network and netuid")]
    Network {
        id: String,
        network: String,
        netuid: u64,
    },
    /// An incentive record switching to a document this build does not embed, or to one under
    /// another digest: the whole history is refused, not only the summaries it reaches.
    #[error("record {id}: {reason}")]
    Document { id: String, reason: String },
    #[error("malformed records: {0}")]
    Json(serde_json::Error),
}

/// The trust a history is checked under: the validator's network, netuid and platform keys.
pub struct Trust<'a> {
    pub network: &'a str,
    pub netuid: u64,
    pub signers: &'a [String],
}

/// The incentive a network starts under: the formula before documents, no verifier payout.
/// A schedule's first incentive record seeds it, and a v4 summary that names no record is
/// scored only by it.
pub const GENESIS_INCENTIVE: &str = "relative-v1";

/// Whether `id` and `digest` are the genesis document this build embeds.
pub fn is_genesis(id: &str, digest: &str) -> bool {
    id == GENESIS_INCENTIVE
        && sn46_shared::incentive::get(GENESIS_INCENTIVE)
            .is_ok_and(|document| document.digest == digest)
}

/// The document an incentive record switches to, as this build embeds it: the record names
/// it by id and by that copy's digest, and the first record of the kind (one moving from no
/// head) seeds the genesis document. Its notice is what the validation table checks the
/// record against. A recipe record names no document of this build (notice 0: the table
/// applies its floor).
fn document_notice(record: &Record) -> Result<u64, ScheduleError> {
    if record.kind != Kind::Incentive {
        return Ok(0);
    }
    let refuse = |reason: String| ScheduleError::Document {
        id: record.id.clone(),
        reason,
    };
    let document =
        sn46_shared::incentive::get(&record.to).map_err(|error| refuse(error.to_string()))?;
    if document.digest != record.to_digest {
        return Err(refuse(format!(
            "names {} under digest {}, this build embeds {}",
            record.to, record.to_digest, document.digest
        )));
    }
    if record.reason == Reason::Normal
        && record.from.is_none()
        && !is_genesis(&record.to, &record.to_digest)
    {
        return Err(refuse(format!(
            "the first incentive record seeds the genesis {GENESIS_INCENTIVE}, not {}",
            record.to
        )));
    }
    Ok(document.notice_epochs)
}

/// Admit `records` after `history`, each checked under `trust`.
pub fn extend(
    history: &mut History,
    records: Vec<Record>,
    trust: &Trust<'_>,
) -> Result<(), ScheduleError> {
    for record in records {
        if record.network != trust.network || record.netuid != trust.netuid {
            return Err(ScheduleError::Network {
                id: record.id,
                network: record.network,
                netuid: record.netuid,
            });
        }
        if !trust.signers.contains(&record.signer) {
            return Err(ScheduleError::Signer {
                id: record.id,
                signer: record.signer,
            });
        }
        record.authenticate(&record.signer)?;
        let at = record.announced_epoch;
        let wants = document_notice(&record)?;
        history.admit(record, at, wants)?;
    }
    Ok(())
}

/// Where the platform serves its schedule's signed history: the summary URL's origin.
pub fn history_url(epoch_summary_url: &str, since: u64) -> Result<String, RunError> {
    let uri: ureq::http::Uri = epoch_summary_url
        .parse()
        .map_err(|error| RunError::Fetch(format!("Platform summary URL is invalid: {error}")))?;
    let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
        return Err(RunError::Fetch(
            "Platform summary URL has no scheme and host".into(),
        ));
    };
    Ok(format!(
        "{scheme}://{authority}/v1/schedule/history?since={since}"
    ))
}

fn cache_path(state: &StateStore) -> PathBuf {
    state.path.with_extension("schedule.json")
}

/// The history kept from earlier runs, replayed again under `trust`; empty when there is none
/// or it no longer replays.
fn load_cached(state: &StateStore, trust: &Trust<'_>) -> History {
    let path = cache_path(state);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return History::default(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Schedule history copy unreadable: fetching it whole");
            return History::default();
        }
    };
    let mut history = History::default();
    let replayed = serde_json::from_slice(&raw)
        .map_err(ScheduleError::Json)
        .and_then(|records| extend(&mut history, records, trust));
    match replayed {
        Ok(()) => history,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Schedule history copy no longer verifies: fetching it whole");
            History::default()
        }
    }
}

fn fetch_records(run: &Run<'_>, since: u64) -> Result<Vec<Record>, RunError> {
    let raw = (run.fetch)(
        &history_url(run.epoch_summary_url, since)?,
        run.timeout,
        Resource::ScheduleHistory,
    )?;
    serde_json::from_slice(&raw).map_err(|error| {
        RunError::Incentive(format!("summary not scored: schedule history: {error}"))
    })
}

/// The platform's history, verified under `trust`: the kept copy extended by the records after
/// it, or (with no copy, or one the platform's records do not continue) the whole history.
pub fn verified(run: &Run<'_>) -> Result<History, RunError> {
    let trust = Trust {
        network: run.network,
        netuid: run.netuid,
        signers: run.platform_signers,
    };
    let refuse = |error: ScheduleError| {
        RunError::Incentive(format!("summary not scored: schedule history: {error}"))
    };
    let cached = load_cached(run.state, &trust);
    let held = cached.records().len();
    let since = cached.records().last().map_or(0, |record| record.revision);
    let mut history = cached;
    let changed = match extend(&mut history, fetch_records(run, since)?, &trust) {
        Ok(()) => history.records().len() != held,
        Err(error) if since == 0 => return Err(refuse(error)),
        Err(error) => {
            tracing::warn!(%error, "Schedule history does not continue the kept copy: fetching it whole");
            history = History::default();
            extend(&mut history, fetch_records(run, 0)?, &trust).map_err(refuse)?;
            true
        }
    };
    if changed {
        let mut data = serde_json::to_vec(history.records())
            .map_err(|error| refuse(ScheduleError::Json(error)))?;
        data.push(b'\n');
        if let Err(error) = save_bytes(&cache_path(run.state), &data) {
            tracing::warn!(%error, "Schedule history copy could not be saved: the next run fetches it whole");
        }
    }
    Ok(history)
}

#[cfg(test)]
pub(crate) mod tests {
    use subxt_signer::{SecretUri, sr25519::Keypair};

    use super::*;

    pub(crate) fn key(uri: &str) -> Keypair {
        Keypair::from_uri(&uri.parse::<SecretUri>().unwrap()).unwrap()
    }

    pub(crate) fn ss58(keypair: &Keypair) -> String {
        keypair.public_key().to_account_id().to_string()
    }

    /// The two-record incentive history the tests replay: a seed putting `relative-v1` in
    /// force at epoch 1 signed by `first`, then a normal switch to `relative-v1-payout`
    /// signed by `second` on `network`.
    pub(crate) fn two_records(first: &Keypair, second: &Keypair, network: &str) -> Vec<Record> {
        let seed = sn46_shared::incentive::get("relative-v1").unwrap();
        let next = sn46_shared::incentive::get("relative-v1-payout").unwrap();
        let mut one = Record {
            activation_ref: None,
            announced_epoch: 1,
            cancels: None,
            digest: String::new(),
            effective_epoch: 1,
            from: None,
            from_digest: None,
            id: "incentive-1".into(),
            kind: Kind::Incentive,
            mode: None,
            netuid: 46,
            network: network.into(),
            predecessor: None,
            reason: Reason::Normal,
            revision: 1,
            signature: String::new(),
            signer: ss58(first),
            to: seed.id.clone(),
            to_digest: seed.digest.clone(),
        };
        one.sign(|bytes| first.sign(bytes).0.to_vec()).unwrap();
        let mut two = Record {
            announced_epoch: 2,
            effective_epoch: 2 + next
                .notice_epochs
                .max(sn46_shared::schedule::MIN_NOTICE_EPOCHS),
            from: Some(seed.id),
            from_digest: Some(seed.digest),
            id: "incentive-2".into(),
            predecessor: Some("incentive-1".into()),
            revision: 2,
            signer: ss58(second),
            to: next.id,
            to_digest: next.digest,
            ..one.clone()
        };
        two.sign(|bytes| second.sign(bytes).0.to_vec()).unwrap();
        vec![one, two]
    }

    /// Seed `relative-v1`, then a normal switch at epoch 2 to `to` (under `to_digest`,
    /// effective 22), then at epoch 22 the revert back to `relative-v1`: a history whose
    /// final head is the genesis document whatever `to` was.
    pub(crate) fn through(keypair: &Keypair, to: &str, to_digest: &str) -> Vec<Record> {
        let mut records = two_records(keypair, keypair, "local");
        records[1].to = to.into();
        records[1].to_digest = to_digest.into();
        records[1]
            .sign(|bytes| keypair.sign(bytes).0.to_vec())
            .unwrap();
        let mut back = Record {
            activation_ref: Some("incentive-2".into()),
            announced_epoch: 22,
            effective_epoch: 22,
            from: Some(to.into()),
            from_digest: Some(to_digest.into()),
            id: "incentive-3".into(),
            predecessor: Some("incentive-2".into()),
            reason: Reason::Revert,
            revision: 3,
            to: records[0].to.clone(),
            to_digest: records[0].to_digest.clone(),
            ..records[0].clone()
        };
        back.sign(|bytes| keypair.sign(bytes).0.to_vec()).unwrap();
        records.push(back);
        records
    }

    /// Every incentive record names a document this build embeds, by id and by that copy's
    /// digest, intermediate ones included, and the first one seeds the genesis document; a
    /// history with any other is refused whole.
    #[test]
    fn every_incentive_record_names_an_embedded_document() {
        let dave = key("//Dave");
        let signers = [ss58(&dave)];
        let payout = sn46_shared::incentive::get("relative-v1-payout").unwrap();
        assert_eq!(
            replay(
                through(&dave, &payout.id, &payout.digest),
                "local",
                &signers
            )
            .unwrap()
            .records()
            .len(),
            3
        );
        for (to, digest, expect) in [
            (
                "relative-v9",
                payout.digest.as_str(),
                "no incentive relative-v9",
            ),
            ("relative-v1-payout", "sha256:00", "under digest sha256:00"),
        ] {
            let error = replay(through(&dave, to, digest), "local", &signers).unwrap_err();
            assert!(
                matches!(&error, ScheduleError::Document { id, reason } if id == "incentive-2" && reason.contains(expect)),
                "{error}"
            );
        }
        // A seed of anything but the genesis document.
        let mut seed = two_records(&dave, &dave, "local");
        seed.truncate(1);
        seed[0].to = payout.id.clone();
        seed[0].to_digest = payout.digest.clone();
        seed[0].sign(|bytes| dave.sign(bytes).0.to_vec()).unwrap();
        let error = replay(seed, "local", &signers).unwrap_err();
        assert!(
            matches!(&error, ScheduleError::Document { id, reason } if id == "incentive-1" && reason.contains("genesis relative-v1")),
            "{error}"
        );
    }

    fn replay(
        records: Vec<Record>,
        network: &str,
        signers: &[String],
    ) -> Result<History, ScheduleError> {
        let mut history = History::default();
        extend(
            &mut history,
            records,
            &Trust {
                network,
                netuid: 46,
                signers,
            },
        )?;
        Ok(history)
    }

    /// After a key rotation the history holds records of both keys: it replays while both are
    /// trusted, and a record of a key no longer trusted is refused by name.
    #[test]
    fn records_are_authenticated_by_the_trusted_keys_of_their_time() {
        let (old, new) = (key("//Dave"), key("//Eve"));
        let records = two_records(&old, &new, "local");
        let both = [ss58(&new), ss58(&old)];
        assert_eq!(
            replay(records.clone(), "local", &both)
                .unwrap()
                .records()
                .len(),
            2
        );
        let error = replay(records.clone(), "local", &both[..1]).unwrap_err();
        assert!(
            matches!(&error, ScheduleError::Signer { id, .. } if id == "incentive-1"),
            "{error}"
        );
        // A record claiming a trusted signer it was not signed by is still refused.
        let mut forged = records;
        forged[1].signer = ss58(&old);
        assert!(matches!(
            replay(forged, "local", &both),
            Err(ScheduleError::Record(_))
        ));
    }

    /// Another deployment's history, even one signed by a trusted key, does not admit.
    #[test]
    fn records_of_another_network_or_netuid_are_refused() {
        let dave = key("//Dave");
        let signers = [ss58(&dave)];
        let error = replay(two_records(&dave, &dave, "finney"), "local", &signers).unwrap_err();
        assert!(
            matches!(&error, ScheduleError::Network { id, .. } if id == "incentive-1"),
            "{error}"
        );
        let mut history = History::default();
        let error = extend(
            &mut history,
            two_records(&dave, &dave, "local"),
            &Trust {
                network: "local",
                netuid: 7,
                signers: &signers,
            },
        )
        .unwrap_err();
        assert!(matches!(error, ScheduleError::Network { netuid: 46, .. }));
    }

    #[test]
    fn the_history_url_is_the_summary_urls_origin() {
        for (summary, history) in [
            (
                "https://host/validator/v1/epoch-summaries/latest",
                "https://host/v1/schedule/history?since=3",
            ),
            (
                "https://host/latest.json",
                "https://host/v1/schedule/history?since=3",
            ),
            (
                "http://127.0.0.1:8080/a/b?x=1",
                "http://127.0.0.1:8080/v1/schedule/history?since=3",
            ),
        ] {
            assert_eq!(history_url(summary, 3).unwrap(), history);
        }
        assert!(history_url("/latest.json", 0).is_err());
    }
}
