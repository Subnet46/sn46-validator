//! One validator run: fetch the summary, validate it against the chain, score, log, burn.

use std::io::Read;
use std::time::Duration;

use crate::burn::{BurnError, BurnFraction, Burner};
use crate::chain::{Chain, ChainError, validate_chain};
use crate::scoring::log_score_records;
use crate::state::{StateError, StateStore, ValidatorState};
use sn46_shared::epoch_summary::{EpochSummaryError, MAX_EPOCH_SUMMARY_BYTES, parse_epoch_summary};
use sn46_shared::scoring::{MinerScore, score_epoch_summary};

/// Any failure `main` logs as `Validator run failed` and maps to exit code 1.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{0}")]
    EpochSummary(#[from] EpochSummaryError),
    #[error("{0}")]
    Chain(#[from] ChainError),
    #[error("{0}")]
    State(#[from] StateError),
    #[error("{0}")]
    Burn(#[from] BurnError),
    /// The platform summary could not be fetched.
    #[error("{0}")]
    Fetch(String),
}

pub type Fetch<'a> = &'a dyn Fn(&str, Duration) -> Result<Vec<u8>, RunError>;

pub fn fetch_epoch_summary(url: &str, timeout: Duration) -> Result<Vec<u8>, RunError> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent("sn46-validator/0.1")
        .http_status_as_error(true)
        .build()
        .new_agent();
    let mut raw = Vec::new();
    agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .and_then(|response| {
            response
                .into_body()
                .into_reader()
                .take(MAX_EPOCH_SUMMARY_BYTES as u64 + 1)
                .read_to_end(&mut raw)
                .map_err(ureq::Error::from)
        })
        .map_err(|error| RunError::Fetch(format!("Platform summary fetch failed: {error}")))?;
    if raw.is_empty() || raw.len() > MAX_EPOCH_SUMMARY_BYTES {
        return Err(RunError::Fetch("Platform summary size is invalid".into()));
    }
    Ok(raw)
}

pub struct Run<'a> {
    pub network: &'a str,
    pub netuid: u64,
    pub epoch_summary_url: &'a str,
    pub platform_signer: &'a str,
    pub chain: &'a dyn Chain,
    pub state: &'a StateStore,
    pub burner: &'a dyn Burner,
    pub timeout: Duration,
    pub fetch: Fetch<'a>,
}

pub fn run_once(run: &Run<'_>) -> Result<Vec<MinerScore>, RunError> {
    let _lock = run.state.lock()?;
    let snapshot = run.chain.snapshot(run.netuid)?;
    let epoch_summary = parse_epoch_summary(
        &(run.fetch)(run.epoch_summary_url, run.timeout)?,
        run.network,
        run.netuid,
        run.platform_signer,
    )?;
    validate_chain(&epoch_summary, &snapshot)?;
    let mut saved = run.state.load()?;
    let summary_id = epoch_summary.summary_id.as_str();
    let digest = epoch_summary.digest.as_str();
    let epoch_summary_end = epoch_summary.epoch_end_block;
    if saved
        .summary_epoch_end_block
        .is_some_and(|end| end > epoch_summary_end)
    {
        return Err(StateError::OlderEpochSummary.into());
    }
    let already_processed = saved.summary_epoch_end_block == Some(epoch_summary_end);
    if already_processed
        && (saved.summary_id.as_deref() != Some(summary_id)
            || saved.summary_digest.as_deref() != Some(digest))
    {
        return Err(StateError::ChangedEpochSummary.into());
    }

    let records = if already_processed {
        tracing::info!("Summary already processed summary_id={summary_id}");
        Vec::new()
    } else {
        let records = score_epoch_summary(&epoch_summary);
        saved = ValidatorState {
            summary_id: Some(summary_id.to_owned()),
            summary_digest: Some(digest.to_owned()),
            summary_epoch_end_block: Some(epoch_summary_end),
            ..saved
        };
        run.state.save(&saved)?;
        log_score_records(&records);
        tracing::info!(
            "Summary verified summary_id={summary_id} finalized_block={} miners={}",
            snapshot.finalized_block,
            records.len()
        );
        records
    };

    if saved.burn_epoch_end_block != Some(epoch_summary_end) {
        let hotkey = run.burner.hotkey();
        let validator_uid = snapshot
            .hotkeys
            .iter()
            .position(|candidate| candidate == hotkey)
            .ok_or_else(|| BurnError("Validator hotkey is not registered".into()))?;
        if snapshot.last_updates[validator_uid] >= epoch_summary_end {
            tracing::info!("Weights already on chain epoch_end_block={epoch_summary_end}");
        } else {
            // Retries still need miner scores for partial or zero burn, without logging them again.
            let retry_records = already_processed.then(|| score_epoch_summary(&epoch_summary));
            let message = run.burner.submit(
                epoch_summary.netuid.get(),
                snapshot.finalized_block,
                retry_records.as_deref().unwrap_or(&records),
                BurnFraction::CURRENT,
            )?;
            tracing::info!(
                "✅ Weight submission finalized epoch_end_block={epoch_summary_end} result={message}"
            );
        }
        run.state.save(&ValidatorState {
            burn_epoch_end_block: Some(epoch_summary_end),
            ..saved
        })?;
    }
    Ok(records)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::{Cell, RefCell};
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::chain::ChainSnapshot;
    use crate::chain::tests::{fixture, fixture_snapshot, snapshot_of};

    const RAW: &[u8] = include_str!("../tests/fixtures/epoch_summary_v2.json").as_bytes();

    fn raw() -> Vec<u8> {
        RAW.strip_suffix(b"\n").unwrap().to_vec()
    }

    pub(crate) struct FixedChain {
        pub snapshot: ChainSnapshot,
        pub netuids: RefCell<Vec<u64>>,
    }

    impl Chain for FixedChain {
        fn snapshot(&self, netuid: u64) -> Result<ChainSnapshot, ChainError> {
            self.netuids.borrow_mut().push(netuid);
            Ok(self.snapshot.clone())
        }
    }

    pub(crate) struct FakeBurner {
        pub hotkey: String,
        pub fail: Cell<bool>,
        pub calls: RefCell<Vec<(u64, u64)>>,
        pub scores: RefCell<Vec<Vec<MinerScore>>>,
    }

    impl FakeBurner {
        fn new(hotkey: &str, fail: bool) -> Self {
            Self {
                hotkey: hotkey.into(),
                fail: Cell::new(fail),
                calls: RefCell::new(vec![]),
                scores: RefCell::new(vec![]),
            }
        }
    }

    impl Burner for FakeBurner {
        fn hotkey(&self) -> &str {
            &self.hotkey
        }
        fn submit(
            &self,
            netuid: u64,
            finalized_block: u64,
            miners: &[MinerScore],
            _burn: BurnFraction,
        ) -> Result<String, BurnError> {
            self.calls.borrow_mut().push((netuid, finalized_block));
            self.scores.borrow_mut().push(miners.to_vec());
            if self.fail.get() {
                return Err(BurnError("failed".into()));
            }
            Ok("finalized".into())
        }
    }

    /// Collects tracing messages, one per line, with no prefix.
    #[derive(Clone, Default)]
    pub(crate) struct Log(Arc<Mutex<Vec<u8>>>);

    impl Log {
        pub(crate) fn messages(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
        pub(crate) fn capture<T>(&self, body: impl FnOnce() -> T) -> T {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_max_level(tracing::Level::DEBUG)
                .with_level(false)
                .with_target(false)
                .without_time()
                .with_ansi(false)
                .finish();
            tracing::subscriber::with_default(subscriber, body)
        }
    }

    impl Write for Log {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Log {
        type Writer = Log;
        fn make_writer(&'a self) -> Log {
            self.clone()
        }
    }

    fn run<'a>(chain: &'a dyn Chain, state: &'a StateStore, burner: &'a dyn Burner) -> Run<'a> {
        Run {
            network: "local",
            netuid: 46,
            epoch_summary_url: "https://platform.invalid/latest",
            platform_signer: "5DAAnrj7VHTznn2AWBemMuyBwZWs6FNFjdyVXUeYum3PTXFy",
            chain,
            state,
            burner,
            timeout: Duration::from_secs(15),
            fetch: &|url, timeout| {
                assert_eq!(url, "https://platform.invalid/latest");
                assert_eq!(timeout, Duration::from_secs(15));
                Ok(raw())
            },
        }
    }

    fn weights(records: &[MinerScore]) -> Vec<u128> {
        records
            .iter()
            .map(|record| record.normalized_weight)
            .collect()
    }

    #[test]
    fn run_once_fetches_authenticates_scores_logs_and_exits() {
        let temp = tempfile::tempdir().unwrap();
        let chain = FixedChain {
            snapshot: fixture_snapshot(),
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
        let log = Log::default();
        let records = log
            .capture(|| run_once(&run(&chain, &store, &burner)))
            .unwrap();
        assert_eq!(*chain.netuids.borrow(), [46]);
        let uids: Vec<u64> = records.iter().map(|record| record.miner.uid).collect();
        assert_eq!(uids, [12, 37, 55]);
        assert_eq!(weights(&records), [65_535, 30_287, 0]);
        let messages = log.messages();
        assert_eq!(
            messages
                .iter()
                .filter(|line| line.contains("miner_score "))
                .count(),
            3
        );
        assert_eq!(
            messages[messages.len() - 2],
            "Summary verified summary_id=local-46-361-720 finalized_block=722 miners=3"
        );
    }

    #[test]
    fn epoch_summary_and_burn_are_each_handled_once() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = fixture_snapshot();
        let burner = FakeBurner::new(&snapshot.hotkeys[12], false);
        let chain = FixedChain {
            snapshot,
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let arguments = run(&chain, &store, &burner);
        let first = run_once(&arguments).unwrap();
        let second = run_once(&arguments).unwrap();
        assert_eq!(first.len(), 3);
        assert!(second.is_empty());
        assert_eq!(*burner.calls.borrow(), [(46, 722)]);
        let saved = store.load().unwrap();
        assert_eq!(saved.summary_id.as_deref(), Some("local-46-361-720"));
        assert_eq!(saved.summary_digest, Some(fixture().digest));
        assert_eq!(saved.summary_epoch_end_block, Some(720));
        assert_eq!(saved.burn_epoch_end_block, Some(720));
    }

    #[test]
    fn stale_or_changed_epoch_summaries_return_typed_state_errors() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let chain = FixedChain {
            snapshot: fixture_snapshot(),
            netuids: RefCell::new(vec![]),
        };
        let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
        let arguments = run(&chain, &store, &burner);
        run_once(&arguments).unwrap();
        let mut saved = store.load().unwrap();
        saved.summary_epoch_end_block = Some(721);
        store.save(&saved).unwrap();
        assert!(matches!(
            run_once(&arguments),
            Err(RunError::State(StateError::OlderEpochSummary))
        ));
        saved.summary_epoch_end_block = Some(720);
        saved.summary_digest = Some("different".into());
        store.save(&saved).unwrap();
        assert!(matches!(
            run_once(&arguments),
            Err(RunError::State(StateError::ChangedEpochSummary))
        ));
    }

    #[test]
    fn chain_last_update_prevents_burn_after_local_state_loss() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = fixture_snapshot();
        snapshot.last_updates[12] = 720;
        let burner = FakeBurner::new(&snapshot.hotkeys[12], false);
        let chain = FixedChain {
            snapshot,
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let log = Log::default();
        log.capture(|| run_once(&run(&chain, &store, &burner)))
            .unwrap();
        assert!(burner.calls.borrow().is_empty());
        assert_eq!(store.load().unwrap().burn_epoch_end_block, Some(720));
        assert_eq!(
            log.messages().last().unwrap(),
            "Weights already on chain epoch_end_block=720"
        );
    }

    #[test]
    fn failed_burn_is_retried_without_reprocessing_epoch_summary() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = fixture_snapshot();
        let burner = FakeBurner::new(&snapshot.hotkeys[12], true);
        let chain = FixedChain {
            snapshot,
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let arguments = run(&chain, &store, &burner);
        assert!(matches!(
            run_once(&arguments).unwrap_err(),
            RunError::Burn(BurnError(message)) if message == "failed"
        ));
        assert_eq!(store.load().unwrap().burn_epoch_end_block, None);
        burner.fail.set(false);
        let log = Log::default();
        let records = log.capture(|| run_once(&arguments)).unwrap();
        assert!(records.is_empty());
        assert_eq!(*burner.calls.borrow(), [(46, 722), (46, 722)]);
        let submissions = burner.scores.borrow();
        assert_eq!(submissions[0], score_epoch_summary(&fixture()));
        assert_eq!(submissions[1], submissions[0]);
        assert_eq!(store.load().unwrap().burn_epoch_end_block, Some(720));
        assert_eq!(
            log.messages(),
            [
                "Summary already processed summary_id=local-46-361-720",
                "✅ Weight submission finalized epoch_end_block=720 result=finalized",
            ]
        );
    }

    #[test]
    fn signed_summary_changes_are_rejected_until_the_next_epoch() {
        use sha2::{Digest, Sha256};
        use sn46_shared::epoch_summary::EpochSummary;
        use subxt_signer::{SecretUri, sr25519::Keypair};

        let signer = Keypair::from_uri(&"//Dave".parse::<SecretUri>().unwrap()).unwrap();
        let sign = |summary: &mut EpochSummary| {
            let payload = summary.signing_payload();
            summary.digest = format!("sha256:{}", hex::encode(Sha256::digest(&payload)));
            summary.signature = hex::encode(signer.sign(payload.as_bytes()).0);
        };
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let original = fixture();
        let previous = ValidatorState {
            summary_id: Some(original.summary_id.clone()),
            summary_digest: Some(original.digest.clone()),
            summary_epoch_end_block: Some(720),
            burn_epoch_end_block: Some(720),
        };
        store.save(&previous).unwrap();
        let mut chain = FixedChain {
            snapshot: fixture_snapshot(),
            netuids: RefCell::new(vec![]),
        };
        let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
        let mut summary = original;
        summary.created_at_ms = std::num::NonZeroU64::new(summary.created_at_ms.get() + 1).unwrap();
        sign(&mut summary);
        let changed = |_: &str, _: Duration| Ok(summary.canonical_json().into_bytes());
        let mut arguments = run(&chain, &store, &burner);
        arguments.fetch = &changed;
        assert!(matches!(
            run_once(&arguments),
            Err(RunError::State(StateError::ChangedEpochSummary))
        ));
        assert_eq!(store.load().unwrap(), previous);
        assert!(burner.calls.borrow().is_empty());

        summary.epoch_start_block = 721;
        summary.epoch_end_block = 1080;
        summary.finalized_block = 1082;
        summary.summary_id = "local-46-721-1080".into();
        sign(&mut summary);
        chain.snapshot.last_step = 1080;
        chain.snapshot.finalized_block = 1082;
        let fetch = |_: &str, _: Duration| Ok(summary.canonical_json().into_bytes());
        let mut arguments = run(&chain, &store, &burner);
        arguments.fetch = &fetch;
        assert_eq!(run_once(&arguments).unwrap().len(), 3);
        assert_eq!(
            store.load().unwrap().summary_id.as_deref(),
            Some("local-46-721-1080")
        );
        assert_eq!(store.load().unwrap().burn_epoch_end_block, Some(1080));
        assert_eq!(*burner.calls.borrow(), [(46, 1082)]);
    }

    /// Two validators and a late restart agree on one result.
    #[test]
    fn two_validators_and_late_restart_produce_one_identical_result() {
        let (first_dir, second_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let chain = FixedChain {
            snapshot: fixture_snapshot(),
            netuids: RefCell::new(vec![]),
        };
        let (first_store, second_store) = (
            StateStore::new(first_dir.path().join("state.json")),
            StateStore::new(second_dir.path().join("state.json")),
        );
        let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
        let first = run_once(&run(&chain, &first_store, &burner)).unwrap();
        let late = run_once(&run(&chain, &second_store, &burner)).unwrap();
        let restarted = run_once(&run(&chain, &second_store, &burner)).unwrap();
        assert_eq!(first, late);
        assert!(restarted.is_empty());
        assert_eq!(weights(&first), [65_535, 30_287, 0]);
        assert_eq!(first[2].score_bps, 0);
        assert!(first[2].disqualified);
    }

    #[test]
    fn golden_log_lines_and_state_transitions() {
        let cases: Value =
            serde_json::from_str(include_str!("../tests/golden/runtime_cases.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let temp = tempfile::tempdir().unwrap();
            let store = StateStore::new(temp.path().join("state.json"));
            for (step, run_case) in case["runs"].as_array().unwrap().iter().enumerate() {
                if let Some(before) = run_case["state_before"].as_object() {
                    store
                        .save(&ValidatorState {
                            summary_id: before["summary_id"].as_str().map(str::to_owned),
                            summary_digest: before["summary_digest"].as_str().map(str::to_owned),
                            summary_epoch_end_block: before["summary_epoch_end_block"].as_u64(),
                            burn_epoch_end_block: before["burn_epoch_end_block"].as_u64(),
                        })
                        .unwrap();
                }
                let spec = &run_case["burner"];
                let burner = FakeBurner::new(
                    spec["hotkey"].as_str().unwrap(),
                    spec["fail"].as_bool().unwrap(),
                );
                let chain = FixedChain {
                    snapshot: snapshot_of(&run_case["snapshot"]),
                    netuids: RefCell::new(vec![]),
                };
                let log = Log::default();
                let result = log.capture(|| run_once(&run(&chain, &store, &burner)));
                let label = format!("{name} step {step}");
                match run_case["verdict"].as_str().unwrap() {
                    "ok" => assert!(result.is_ok(), "{label}: {result:?}"),
                    _ => assert_eq!(
                        result.unwrap_err().to_string(),
                        run_case["message"],
                        "{label}"
                    ),
                }
                assert_eq!(json!(log.messages()), run_case["messages"], "{label}");
                let calls: Vec<Value> = burner
                    .calls
                    .borrow()
                    .iter()
                    .map(|(n, b)| json!([n, b]))
                    .collect();
                assert_eq!(Value::Array(calls), spec["calls"], "{label}");
                let after = store.load().unwrap();
                assert_eq!(
                    json!({
                        "summary_id": after.summary_id,
                        "summary_digest": after.summary_digest,
                        "summary_epoch_end_block": after.summary_epoch_end_block,
                        "burn_epoch_end_block": after.burn_epoch_end_block,
                    }),
                    run_case["state_after"],
                    "{label}"
                );
            }
        }
    }
}
