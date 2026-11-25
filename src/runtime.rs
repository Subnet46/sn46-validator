//! One validator run: fetch the summary, validate it against the chain, score, log, burn.

use std::io::Read;
use std::time::Duration;

use crate::burn::{BurnError, BurnPolicy, Burner};
use crate::chain::{Chain, ChainError, ChainSnapshot, check_verifiers, validate_chain};
use crate::scoring::log_score_records;
use crate::state::{StateError, StateStore, ValidatorState};
use crate::transitions::{Head, RecordRef, Transitions, TransitionsStore, check_head};
use sn46_shared::epoch_summary::{
    EpochSummary, EpochSummaryError, MAX_EPOCH_SUMMARY_BYTES, parse_epoch_summary,
};
use sn46_shared::scoring::MinerScore;
#[cfg(test)]
use sn46_shared::scoring::score_epoch_summary;

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
    /// The summary names an incentive this build does not embed, or one under another
    /// digest: it is not scored and no weights are set.
    #[error("{0}")]
    Incentive(String),
}

/// A v4 summary's incentive checked against the platform's signed history
/// ([`crate::schedule::verified`]: every record signed by a trusted platform key, for this
/// network and netuid, chained, admitted by the validation table, each naming a document this
/// build embeds). The record in force at the summary's epoch must be the one the summary names,
/// switching to the document it names. A summary that names none is scored only while no
/// incentive record is in force at its epoch, and only by the genesis `relative-v1` (by id and
/// digest): a payout document is reachable only through a signed, noticed record. v2 and v3
/// summaries name no incentive and are not checked here. Anything else sets no weights.
fn check_transition(
    named: &sn46_shared::epoch_summary::IncentiveRef,
    epoch: u64,
    history: &sn46_shared::schedule::History,
) -> Result<Option<sn46_shared::schedule::Record>, RunError> {
    let refuse = |reason: String| RunError::Incentive(format!("summary not scored: {reason}"));
    let in_force = history.in_force(sn46_shared::schedule::Kind::Incentive, epoch);
    match (&named.transition, in_force) {
        (None, None) => {
            if crate::schedule::is_genesis(&named.id, &named.digest) {
                Ok(None)
            } else {
                Err(refuse(format!(
                    "the summary names {} ({}) with no schedule record; only the genesis {} is in force without one",
                    named.id,
                    named.digest,
                    crate::schedule::GENESIS_INCENTIVE
                )))
            }
        }
        (None, Some((record, id))) => Err(refuse(format!(
            "the summary names no schedule record, the schedule has {} ({id}) in force at epoch {epoch}",
            record.id
        ))),
        (Some(transition), None) => Err(refuse(format!(
            "the summary names {transition}, no incentive record is in force at epoch {epoch}"
        ))),
        (Some(transition), Some((record, id))) => {
            if &record.id != transition || id != named.id || record.to_digest != named.digest {
                return Err(refuse(format!(
                    "the summary names {transition} ({}), the schedule has {} ({id}) in force at epoch {epoch}",
                    named.id, record.id
                )));
            }
            Ok(Some(record.clone()))
        }
    }
}

/// The incentive a summary is scored by ([`sn46_shared::incentive::of_summary`]); a summary
/// whose incentive this build does not embed under that digest is not scored.
pub fn incentive_of(
    epoch_summary: &EpochSummary,
) -> Result<sn46_shared::incentive::Incentive, RunError> {
    sn46_shared::incentive::of_summary(epoch_summary)
        .map_err(|error| RunError::Incentive(format!("summary not scored: {error}")))
}

/// What a fetch reads from the platform, each with its own size ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Summary,
    ScheduleHistory,
}

impl std::fmt::Display for Resource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Summary => "summary",
            Self::ScheduleHistory => "schedule history",
        })
    }
}

/// The largest schedule history response read. Runs fetch only the records after the kept
/// copy (`?since=`), so this bounds the first, whole fetch: some 20,000 records.
pub const MAX_SCHEDULE_HISTORY_BYTES: usize = 16 * 1024 * 1024;

impl Resource {
    pub fn limit(self) -> usize {
        match self {
            Self::Summary => MAX_EPOCH_SUMMARY_BYTES,
            Self::ScheduleHistory => MAX_SCHEDULE_HISTORY_BYTES,
        }
    }
}

/// A GET of `url` within the timeout, read up to the resource's ceiling, with the readiness
/// header ([`crate::readiness`]) when one is given.
pub type Fetch<'a> =
    &'a dyn Fn(&str, Duration, Resource, Option<&str>) -> Result<Vec<u8>, RunError>;

/// How soon after a stale summary to look again. The platform publishes each epoch a few
/// seconds after the chain steps; a poll that lands in that gap sees last epoch's summary.
pub const STALE_RETRY: Duration = Duration::from_secs(30);

/// The wait before the next run. A stale summary is retried after [`STALE_RETRY`]: with a
/// poll interval that is a whole number of epochs (1320 s against the localnet's 132 s)
/// every poll landed in the same gap and failed, so the validator never scored a summary
/// until it was restarted. Anything else waits the configured interval.
pub fn next_delay(outcome: &Result<(), RunError>, poll_interval: Duration) -> Duration {
    match outcome {
        Err(RunError::Chain(ChainError::StaleEpoch)) => STALE_RETRY.min(poll_interval),
        _ => poll_interval,
    }
}

pub fn fetch_platform(
    url: &str,
    timeout: Duration,
    resource: Resource,
    readiness: Option<&str>,
) -> Result<Vec<u8>, RunError> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!("sn46-validator/", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(true)
        .build()
        .new_agent();
    let limit = resource.limit();
    let mut raw = Vec::new();
    let mut request = agent.get(url).header("Accept", "application/json");
    if let Some(readiness) = readiness {
        request = request.header(crate::readiness::HEADER, readiness);
    }
    request
        .call()
        .and_then(|response| {
            response
                .into_body()
                .into_reader()
                .take(limit as u64 + 1)
                .read_to_end(&mut raw)
                .map_err(ureq::Error::from)
        })
        .map_err(|error| RunError::Fetch(format!("Platform {resource} fetch failed: {error}")))?;
    if raw.is_empty() || raw.len() > limit {
        return Err(RunError::Fetch(format!(
            "Platform {resource} size is invalid"
        )));
    }
    Ok(raw)
}

pub struct Run<'a> {
    pub network: &'a str,
    pub netuid: u64,
    pub epoch_summary_url: &'a str,
    /// The platform keys a summary may be signed by (more than one while a key rotates).
    pub platform_signers: &'a [String],
    pub chain: &'a dyn Chain,
    pub state: &'a StateStore,
    pub burner: &'a dyn Burner,
    pub timeout: Duration,
    pub fetch: Fetch<'a>,
}

/// The burn the summary's signed miner target asks for. A summary without one (schema v2)
/// sets no weights: there is no fixed fraction to fall back on.
fn target_policy(epoch_summary: &EpochSummary) -> Result<BurnPolicy, BurnError> {
    match (
        epoch_summary.miner_target_usd_cents,
        epoch_summary.tao_price_usd_cents,
    ) {
        (Some(target), Some(price)) => Ok(BurnPolicy::Target {
            miner_target_usd_cents: target.get(),
            tao_price_usd_cents: price.get(),
        }),
        _ => Err(BurnError("Summary carries no miner target".into())),
    }
}

/// The summary, verified as signed by one of `signers`: the one its `signer` names. A summary
/// any other key signed is refused as `Signer`, as with a single trusted key.
fn parse_signed_summary(
    raw: &[u8],
    network: &str,
    netuid: u64,
    signers: &[String],
) -> Result<EpochSummary, EpochSummaryError> {
    for signer in signers {
        match parse_epoch_summary(raw, network, netuid, signer) {
            Err(EpochSummaryError::Signer) => continue,
            outcome => return outcome,
        }
    }
    Err(EpochSummaryError::Signer)
}

pub fn run_once(run: &Run<'_>) -> Result<Vec<MinerScore>, RunError> {
    let _lock = run.state.lock()?;
    let snapshot = run.chain.snapshot(run.netuid)?;
    let issued_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let readiness = crate::readiness::header(run.burner.hotkey(), issued_at_ms, |bytes| {
        run.burner.sign(bytes)
    });
    let epoch_summary = parse_signed_summary(
        &(run.fetch)(
            run.epoch_summary_url,
            run.timeout,
            Resource::Summary,
            Some(&readiness),
        )?,
        run.network,
        run.netuid,
        run.platform_signers,
    )?;
    validate_chain(&epoch_summary, &snapshot)?;
    let incentive = incentive_of(&epoch_summary)?;
    let mut saved = run.state.load()?;
    let transitions = TransitionsStore::beside(run.state);
    let head = transitions.reconcile(&saved)?;
    // The record this summary is scored under, and the head it moves from.
    let (scored_under, held) = match &epoch_summary.incentive {
        Some(named) => {
            let history = crate::schedule::verified(run)?;
            let tempo = epoch_summary.tempo.get();
            let epoch = epoch_summary.epoch_end_block / tempo;
            let record = check_transition(named, epoch, &history)?;
            let held = head.resolve(
                &history,
                saved.summary_epoch_end_block.map(|end| end / tempo),
            );
            check_head(held.as_ref(), record.as_ref(), &history)
                .map_err(|error| RunError::Incentive(format!("summary not scored: {error}")))?;
            (record.as_ref().map(RecordRef::from), held)
        }
        // v2 and v3 name no record: the head stays.
        None => {
            let held = match head {
                Head::Known(held) => held,
                Head::Derive => None,
            };
            (held.clone(), held)
        }
    };
    // A document without verifier payout pays the listed verifiers nothing.
    let verifiers = if incentive.verifier_payout {
        check_verifiers(&epoch_summary, &snapshot)
    } else {
        Vec::new()
    };
    for stale in verifiers.iter().filter(|checked| !checked.on_chain) {
        tracing::warn!(
            "Verifier not paid: UID {} does not hold hotkey {} on the finalized chain",
            stale.verifier.uid,
            stale.verifier.hotkey
        );
    }
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
        let records = incentive.score(&epoch_summary);
        // Before the state commits the summary: a run stopped between the two leaves the
        // transitions ahead, and the next one rolls them back to `previous`.
        transitions.save(&Transitions {
            head: scored_under,
            previous: held,
            summary_digest: digest.to_owned(),
            summary_epoch_end_block: epoch_summary_end,
            summary_id: summary_id.to_owned(),
        })?;
        saved = ValidatorState {
            summary_id: Some(summary_id.to_owned()),
            summary_digest: Some(digest.to_owned()),
            summary_epoch_end_block: Some(epoch_summary_end),
            ..saved
        };
        run.state.save(&saved)?;
        log_score_records(&records);
        tracing::info!(
            "Summary verified summary_id={summary_id} finalized_block={} miners={} signer={} incentive={} digest={}",
            snapshot.finalized_block,
            records.len(),
            epoch_summary.signer,
            incentive.id,
            incentive.digest
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
        } else if let Some(allowed_from) = rate_limited(&snapshot, validator_uid) {
            // The chain would reject the extrinsic and the pending record would then lock
            // submissions for a whole mortal era, so wait quietly: the next poll retries,
            // and a newer summary supersedes this epoch.
            tracing::info!(
                "Weights rate-limited last_update={} limit={} allowed_from_block={allowed_from} finalized_block={}",
                snapshot.last_updates[validator_uid],
                snapshot.weights_rate_limit,
                snapshot.finalized_block
            );
            return Ok(records);
        } else {
            let burn = target_policy(&epoch_summary)?;
            // Retries still need miner scores for partial or zero burn, without logging them again.
            let retry_records = already_processed.then(|| incentive.score(&epoch_summary));
            let message = run.burner.submit(
                epoch_summary.netuid.get(),
                snapshot.finalized_block,
                retry_records.as_deref().unwrap_or(&records),
                &verifiers,
                burn,
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

/// The first block at which `WeightsSetRateLimit` lets this validator set weights again,
/// when that is still ahead of the finalized head. A validator that never set weights is
/// never limited.
fn rate_limited(snapshot: &ChainSnapshot, validator_uid: usize) -> Option<u64> {
    let last_update = snapshot.last_updates[validator_uid];
    let allowed_from = last_update.saturating_add(snapshot.weights_rate_limit);
    (last_update > 0 && allowed_from > snapshot.finalized_block).then_some(allowed_from)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::{Cell, RefCell};

    #[test]
    fn a_stale_summary_is_retried_soon_and_anything_else_waits_the_interval() {
        let poll = Duration::from_secs(1320);
        let stale = Err(RunError::Chain(ChainError::StaleEpoch));
        assert_eq!(next_delay(&stale, poll), STALE_RETRY);
        assert_eq!(
            next_delay(&stale, Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(next_delay(&Ok(()), poll), poll);
        assert_eq!(next_delay(&Err(RunError::Fetch("down".into())), poll), poll);
    }
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::chain::CheckedVerifier;
    use crate::chain::tests::{fixture, fixture_snapshot, snapshot_of};

    const RAW: &[u8] = include_str!("../tests/fixtures/epoch_summary_v3.json").as_bytes();

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
        pub burns: RefCell<Vec<BurnPolicy>>,
        pub verifiers: RefCell<Vec<Vec<CheckedVerifier>>>,
    }

    impl FakeBurner {
        fn new(hotkey: &str, fail: bool) -> Self {
            Self {
                hotkey: hotkey.into(),
                fail: Cell::new(fail),
                calls: RefCell::new(vec![]),
                scores: RefCell::new(vec![]),
                burns: RefCell::new(vec![]),
                verifiers: RefCell::new(vec![]),
            }
        }
    }

    impl Burner for FakeBurner {
        fn hotkey(&self) -> &str {
            &self.hotkey
        }
        fn sign(&self, _: &[u8]) -> [u8; 64] {
            [0; 64]
        }
        fn submit(
            &self,
            netuid: u64,
            finalized_block: u64,
            miners: &[MinerScore],
            verifiers: &[CheckedVerifier],
            burn: BurnPolicy,
        ) -> Result<String, BurnError> {
            self.calls.borrow_mut().push((netuid, finalized_block));
            self.verifiers.borrow_mut().push(verifiers.to_vec());
            self.burns.borrow_mut().push(burn);
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

    /// The fixture summaries' signer (`//Dave`).
    const DAVE_SS58: &str = "5DAAnrj7VHTznn2AWBemMuyBwZWs6FNFjdyVXUeYum3PTXFy";
    static DAVE: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| vec![DAVE_SS58.to_owned()]);

    fn ss58_of(uri: &str) -> String {
        use sn46_shared::identity::{SS58_FORMAT, encode_ss58};
        use subxt_signer::{SecretUri, sr25519::Keypair};
        let keypair = Keypair::from_uri(&uri.parse::<SecretUri>().unwrap()).unwrap();
        encode_ss58(&keypair.public_key().0, SS58_FORMAT)
    }

    /// While the platform key rotates, a summary signed by either listed key is accepted
    /// and the log names the one that signed it; a key not on the list is refused.
    #[test]
    fn a_summary_signed_by_any_listed_signer_is_accepted_and_named() {
        let eve = ss58_of("//Eve");
        for signers in [
            vec![DAVE_SS58.to_owned(), eve.clone()],
            vec![eve.clone(), DAVE_SS58.to_owned()],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let chain = FixedChain {
                snapshot: fixture_snapshot(),
                netuids: RefCell::new(vec![]),
            };
            let store = StateStore::new(temp.path().join("state.json"));
            let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
            let mut arguments = run(&chain, &store, &burner);
            arguments.platform_signers = &signers;
            let log = Log::default();
            let records = log.capture(|| run_once(&arguments)).unwrap();
            assert_eq!(records.len(), 3, "{signers:?}");
            assert!(
                log.messages()
                    .iter()
                    .any(|line| line.starts_with("Summary verified ")
                        && line.contains(&format!(" signer={DAVE_SS58} "))),
                "{signers:?}"
            );
        }
        for signers in [vec![eve.clone()], vec![eve, ss58_of("//Ferdie")], vec![]] {
            let temp = tempfile::tempdir().unwrap();
            let chain = FixedChain {
                snapshot: fixture_snapshot(),
                netuids: RefCell::new(vec![]),
            };
            let store = StateStore::new(temp.path().join("state.json"));
            let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
            let mut arguments = run(&chain, &store, &burner);
            arguments.platform_signers = &signers;
            assert!(
                matches!(
                    run_once(&arguments),
                    Err(RunError::EpochSummary(EpochSummaryError::Signer))
                ),
                "{signers:?}"
            );
            assert!(burner.calls.borrow().is_empty());
            assert_eq!(store.load().unwrap().summary_epoch_end_block, None);
        }
    }

    fn run<'a>(chain: &'a dyn Chain, state: &'a StateStore, burner: &'a dyn Burner) -> Run<'a> {
        Run {
            network: "local",
            netuid: 46,
            epoch_summary_url: "https://platform.invalid/latest",
            platform_signers: &DAVE,
            chain,
            state,
            burner,
            timeout: Duration::from_secs(15),
            fetch: &|url, timeout, _, _| {
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
        // uid 55 failed one proof of 80, which the scoring forgives.
        assert_eq!(weights(&records), [65_535, 30_287, 54_409]);
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
            format!(
                "Summary verified summary_id=local-46-361-720 finalized_block=722 miners=3 signer=5DAAnrj7VHTznn2AWBemMuyBwZWs6FNFjdyVXUeYum3PTXFy incentive=relative-v1 digest={}",
                sn46_shared::incentive::get("relative-v1").unwrap().digest
            )
        );
    }

    #[test]
    fn a_summary_without_a_miner_target_is_scored_but_sets_no_weights() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = fixture_snapshot();
        let burner = FakeBurner::new(&snapshot.hotkeys[12], false);
        let chain = FixedChain {
            snapshot,
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let v2 = |_: &str, _: Duration, _: Resource, _: Option<&str>| {
            Ok(include_str!("../tests/fixtures/epoch_summary_v2.json")
                .trim_end()
                .as_bytes()
                .to_vec())
        };
        let mut arguments = run(&chain, &store, &burner);
        arguments.fetch = &v2;
        assert_eq!(
            run_once(&arguments).unwrap_err().to_string(),
            "Summary carries no miner target"
        );
        assert!(burner.calls.borrow().is_empty());
        let saved = store.load().unwrap();
        assert_eq!(saved.summary_epoch_end_block, Some(720));
        assert_eq!(saved.burn_epoch_end_block, None);
    }

    /// A v4 summary is scored by the incentive it names, checked by digest against the
    /// embedded copy; an unknown one, or another digest, is not scored; v2 and v3 are
    /// `relative-v1`.
    #[test]
    fn a_summary_is_scored_by_the_incentive_it_names() {
        let v4: EpochSummary = serde_json::from_str(
            include_str!("../tests/fixtures/epoch_summary_v4.json").trim_end(),
        )
        .unwrap();
        let named = incentive_of(&v4).unwrap();
        assert_eq!(named.id, "relative-v1-payout");
        assert!(named.verifier_payout);
        let mut unknown = v4.clone();
        unknown.incentive.as_mut().unwrap().id = "relative-v9".into();
        assert!(
            incentive_of(&unknown)
                .unwrap_err()
                .to_string()
                .contains("not scored")
        );
        let mut other = v4.clone();
        other.incentive.as_mut().unwrap().digest = "sha256:00".into();
        assert!(
            incentive_of(&other)
                .unwrap_err()
                .to_string()
                .contains("digest")
        );
        let v3: EpochSummary = serde_json::from_str(
            include_str!("../tests/fixtures/epoch_summary_v3.json").trim_end(),
        )
        .unwrap();
        let legacy = incentive_of(&v3).unwrap();
        assert_eq!(legacy.id, "relative-v1");
        assert!(!legacy.verifier_payout);
        assert_eq!(legacy.score(&v3), score_epoch_summary(&v3));
    }

    /// Each run fetches only the records after the copy it kept; a copy that no longer
    /// verifies, or that the platform's records do not continue, is dropped for a whole fetch.
    #[test]
    fn the_schedule_history_is_fetched_after_the_kept_copy() {
        use crate::schedule::tests::{key, two_records};
        let temp = tempfile::tempdir().unwrap();
        let chain = FixedChain {
            snapshot: fixture_snapshot(),
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
        let all = two_records(&key("//Dave"), &key("//Dave"), "local");
        let published = Cell::new(1);
        let ignore_since = Cell::new(false);
        let urls = RefCell::new(Vec::new());
        let fetch = |url: &str, _: Duration, resource: Resource, _: Option<&str>| {
            assert_eq!(resource, Resource::ScheduleHistory);
            urls.borrow_mut().push(url.to_owned());
            let since: u64 = url.rsplit_once("?since=").unwrap().1.parse().unwrap();
            let since = if ignore_since.get() { 0 } else { since };
            let after: Vec<_> = all[..published.get()]
                .iter()
                .filter(|record| record.revision > since)
                .collect();
            Ok(serde_json::to_vec(&after).unwrap())
        };
        let mut arguments = run(&chain, &store, &burner);
        arguments.fetch = &fetch;
        let held = || {
            crate::schedule::verified(&arguments)
                .unwrap()
                .records()
                .len()
        };
        assert_eq!(held(), 1);
        published.set(2);
        assert_eq!(held(), 2);
        assert_eq!(held(), 2);
        let since = |url: &String| url.rsplit_once('=').unwrap().1.to_owned();
        assert_eq!(
            urls.borrow().iter().map(since).collect::<Vec<_>>(),
            ["0", "1", "2"]
        );
        assert!(urls.borrow()[0].starts_with("https://platform.invalid/v1/schedule/history"));
        // The platform answers with records the kept copy already holds: a whole fetch.
        ignore_since.set(true);
        urls.borrow_mut().clear();
        assert_eq!(held(), 2);
        assert_eq!(
            urls.borrow().iter().map(since).collect::<Vec<_>>(),
            ["2", "0"]
        );
        // A kept copy that no longer verifies is dropped.
        ignore_since.set(false);
        urls.borrow_mut().clear();
        std::fs::write(temp.path().join("state.schedule.json"), b"[{}]").unwrap();
        assert_eq!(held(), 2);
        assert_eq!(urls.borrow().iter().map(since).collect::<Vec<_>>(), ["0"]);
        // A key no longer trusted invalidates the copy and the history alike.
        let eve = [crate::schedule::tests::ss58(&key("//Eve"))];
        arguments.platform_signers = &eve;
        assert!(crate::schedule::verified(&arguments).is_err());
    }

    /// A summary naming a schedule record not in force (or none at all) is not scored.
    #[test]
    fn a_summary_naming_a_record_not_in_force_is_not_scored() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = fixture_snapshot();
        snapshot.hotkeys.resize(91, "unused".into());
        snapshot.last_updates.resize(91, 0);
        let burner = FakeBurner::new(&snapshot.hotkeys[12], false);
        let chain = FixedChain {
            snapshot,
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let empty = |_: &str, _: Duration, resource: Resource, _: Option<&str>| {
            if resource == Resource::ScheduleHistory {
                return Ok(b"[]".to_vec());
            }
            Ok(include_str!("../tests/fixtures/epoch_summary_v4.json")
                .trim_end()
                .as_bytes()
                .to_vec())
        };
        let mut arguments = run(&chain, &store, &burner);
        arguments.fetch = &empty;
        let error = run_once(&arguments).unwrap_err().to_string();
        assert!(error.contains("no incentive record is in force"), "{error}");
        assert!(burner.calls.borrow().is_empty());
    }

    /// The v4 fixture moved to the epoch ending at `epoch_end` (tempo 360), naming `id` (by
    /// its embedded digest) and `transition`, signed again by the fixture's key (//Dave).
    pub(crate) fn v4_summary(epoch_end: u64, id: &str, transition: Option<&str>) -> EpochSummary {
        use sha2::{Digest, Sha256};
        let mut summary: EpochSummary = serde_json::from_str(
            include_str!("../tests/fixtures/epoch_summary_v4.json").trim_end(),
        )
        .unwrap();
        summary.epoch_start_block = epoch_end - 359;
        summary.epoch_end_block = epoch_end;
        summary.finalized_block = epoch_end + 2;
        summary.summary_id = format!("local-46-{}-{epoch_end}", epoch_end - 359);
        summary.incentive = Some(sn46_shared::epoch_summary::IncentiveRef {
            digest: sn46_shared::incentive::get(id).unwrap().digest,
            id: id.into(),
            transition: transition.map(str::to_owned),
        });
        let keypair = crate::schedule::tests::key("//Dave");
        let payload = summary.signing_payload().unwrap();
        summary.digest = format!("sha256:{}", hex::encode(Sha256::digest(&payload)));
        summary.signature = hex::encode(keypair.sign(payload.as_bytes()).0);
        summary
    }

    /// A chain at `summary`'s epoch whose roster holds its miners and UID 90.
    pub(crate) fn chain_at(summary: &EpochSummary) -> FixedChain {
        let mut snapshot = fixture_snapshot();
        snapshot.hotkeys.resize(91, "unused".into());
        snapshot.last_updates.resize(91, 0);
        snapshot.last_step = summary.epoch_end_block;
        snapshot.finalized_block = summary.finalized_block;
        FixedChain {
            snapshot,
            netuids: RefCell::new(vec![]),
        }
    }

    /// Runs `summary` against `history` (signed records, served whole) on a fresh state.
    fn score_against(
        summary: &EpochSummary,
        history: &[sn46_shared::schedule::Record],
    ) -> Result<Vec<MinerScore>, RunError> {
        let temp = tempfile::tempdir().unwrap();
        let chain = chain_at(summary);
        let store = StateStore::new(temp.path().join("state.json"));
        let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
        let body = summary.canonical_json().unwrap().into_bytes();
        let records = serde_json::to_vec(history).unwrap();
        let fetch = move |_: &str, _: Duration, resource: Resource, _: Option<&str>| {
            Ok(match resource {
                Resource::Summary => body.clone(),
                Resource::ScheduleHistory => records.clone(),
            })
        };
        let mut arguments = run(&chain, &store, &burner);
        arguments.fetch = &fetch;
        run_once(&arguments)
    }

    /// `transition: None` belongs to v2 and v3. A v4 summary that names no record is scored
    /// only while none is in force at its epoch, and only by the genesis `relative-v1` (id and
    /// digest): a payout document never reaches a summary without its signed, noticed record.
    #[test]
    fn a_v4_summary_without_a_transition_is_only_the_genesis_before_any_record() {
        use crate::schedule::tests::{key, two_records};
        let dave = key("//Dave");
        let history = two_records(&dave, &dave, "local");
        let genesis = v4_summary(720, "relative-v1", None);
        assert_eq!(score_against(&genesis, &[]).unwrap().len(), 3);
        // A seed announced later is not in force yet: the genesis still stands.
        let mut later = history[..1].to_vec();
        later[0].announced_epoch = 5;
        later[0].effective_epoch = 5;
        later[0].sign(|bytes| dave.sign(bytes).0.to_vec()).unwrap();
        assert_eq!(score_against(&genesis, &later).unwrap().len(), 3);
        // The payout document with no record behind it, even before any record.
        let payout = v4_summary(720, "relative-v1-payout", None);
        let error = score_against(&payout, &[]).unwrap_err().to_string();
        assert!(error.contains("only the genesis relative-v1"), "{error}");
        // A record in force: the summary must name it.
        let error = score_against(&genesis, &history[..1])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("names no schedule record") && error.contains("incentive-1"),
            "{error}"
        );
        assert_eq!(
            score_against(
                &v4_summary(720, "relative-v1", Some("incentive-1")),
                &history
            )
            .unwrap()
            .len(),
            3
        );
        // At epoch 22 the payout switch is in force.
        let error = score_against(&v4_summary(7920, "relative-v1-payout", None), &history)
            .unwrap_err()
            .to_string();
        assert!(error.contains("incentive-2"), "{error}");
        score_against(
            &v4_summary(7920, "relative-v1-payout", Some("incentive-2")),
            &history,
        )
        .unwrap();
    }

    /// A valid final summary does not make a history valid: one intermediate record switched
    /// to a document this build does not embed (or under another digest), and the summary
    /// scored by the genesis document the revert after it returned to is still refused.
    #[test]
    fn a_valid_final_summary_after_a_bad_intermediate_record_is_not_scored() {
        let dave = crate::schedule::tests::key("//Dave");
        let payout = sn46_shared::incentive::get("relative-v1-payout").unwrap();
        let summary = v4_summary(8280, "relative-v1", Some("incentive-3"));
        let good = crate::schedule::tests::through(&dave, &payout.id, &payout.digest);
        assert_eq!(score_against(&summary, &good).unwrap().len(), 3);
        for bad in [
            crate::schedule::tests::through(&dave, "relative-v9", &payout.digest),
            crate::schedule::tests::through(&dave, &payout.id, "sha256:00"),
        ] {
            let error = score_against(&summary, &bad).unwrap_err().to_string();
            assert!(
                error.contains("summary not scored") && error.contains("incentive-2"),
                "{error}"
            );
        }
    }

    /// A run over `summary` and `history` on `store`, the chain at the summary's epoch.
    fn run_on(
        store: &StateStore,
        summary: &EpochSummary,
        history: &[sn46_shared::schedule::Record],
    ) -> Result<Vec<MinerScore>, RunError> {
        let chain = chain_at(summary);
        let burner = FakeBurner::new(&chain.snapshot.hotkeys[12], false);
        let body = summary.canonical_json().unwrap().into_bytes();
        let records = serde_json::to_vec(history).unwrap();
        let fetch = move |_: &str, _: Duration, resource: Resource, _: Option<&str>| {
            Ok(match resource {
                Resource::Summary => body.clone(),
                Resource::ScheduleHistory => records.clone(),
            })
        };
        let mut arguments = run(&chain, store, &burner);
        arguments.fetch = &fetch;
        run_once(&arguments)
    }

    fn reference(record: &sn46_shared::schedule::Record) -> Option<RecordRef> {
        Some(RecordRef::from(record))
    }

    /// The validator killed at every write and rename boundary of a run (the schedule copy,
    /// the transitions, the state, the burn record) starts again on the same files and ends
    /// where an uninterrupted run ends. A kill between the transitions and the state leaves the
    /// head ahead: it rolls back, so a platform that lost the newer record (restored, serving
    /// the summary the state holds) is still followed.
    #[test]
    fn a_kill_at_any_write_boundary_recovers_on_restart() {
        use crate::schedule::tests::{key, two_records};
        use crate::state::crash;
        let dave = key("//Dave");
        let history = two_records(&dave, &dave, "local");
        let first = v4_summary(720, "relative-v1", Some("incentive-1"));
        let second = v4_summary(7920, "relative-v1-payout", Some("incentive-2"));
        let mut rolled_back = 0;
        let mut at = 0;
        loop {
            let temp = tempfile::tempdir().unwrap();
            let store = StateStore::new(temp.path().join("state.json"));
            let transitions = TransitionsStore::beside(&store);
            run_on(&store, &first, &history).unwrap();
            crash::arm(Some(at));
            let killed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_on(&store, &second, &history)
            }))
            .is_err();
            crash::arm(None);
            if !killed {
                assert!(at >= 6, "only {at} boundaries");
                break;
            }
            let ahead = store.load().unwrap().summary_id.as_deref() == Some("local-46-361-720")
                && transitions
                    .load()
                    .unwrap()
                    .is_some_and(|written| written.summary_id == second.summary_id);
            if ahead {
                // The platform restored without incentive-2, still serving the first summary.
                rolled_back += 1;
                let restored = TransitionsStore::beside(&store);
                run_on(&store, &first, &history[..1]).unwrap();
                assert_eq!(
                    restored.load().unwrap().unwrap().head,
                    reference(&history[0]),
                    "boundary {at}"
                );
            }
            run_on(&store, &second, &history).unwrap();
            let state = store.load().unwrap();
            assert_eq!(
                state.summary_id.as_deref(),
                Some(second.summary_id.as_str())
            );
            assert_eq!(state.burn_epoch_end_block, Some(7920), "boundary {at}");
            assert_eq!(
                transitions.load().unwrap().unwrap(),
                Transitions {
                    head: reference(&history[1]),
                    previous: reference(&history[0]),
                    summary_digest: second.digest.clone(),
                    summary_epoch_end_block: 7920,
                    summary_id: second.summary_id.clone(),
                },
                "boundary {at}"
            );
            at += 1;
        }
        // After the transitions' rename, and before the state's write and its rename.
        assert_eq!(rolled_back, 3);
        assert_eq!(crash::passed(), 0);
    }

    /// A state an older release advanced (no transitions of its own) has its head derived
    /// again at its epoch; a history that no longer holds the head is refused.
    #[test]
    fn a_head_is_derived_after_an_older_release_and_must_stay_in_the_history() {
        use crate::schedule::tests::{key, two_records};
        let dave = key("//Dave");
        let history = two_records(&dave, &dave, "local");
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state.json"));
        let transitions = TransitionsStore::beside(&store);
        run_on(
            &store,
            &v4_summary(720, "relative-v1", Some("incentive-1")),
            &history,
        )
        .unwrap();
        // The older release scored the epoch-22 summary and wrote only its state.
        let second = v4_summary(7920, "relative-v1-payout", Some("incentive-2"));
        store
            .save(&ValidatorState {
                summary_id: Some(second.summary_id.clone()),
                summary_digest: Some(second.digest.clone()),
                summary_epoch_end_block: Some(7920),
                burn_epoch_end_block: Some(7920),
            })
            .unwrap();
        let third = v4_summary(8280, "relative-v1-payout", Some("incentive-2"));
        run_on(&store, &third, &history).unwrap();
        let written = transitions.load().unwrap().unwrap();
        assert_eq!(written.previous, reference(&history[1]));
        assert_eq!(written.head, reference(&history[1]));
        // The platform's history now lacks incentive-2 (the kept copy gone too, so it is
        // fetched whole): the genesis summary of the fresh history does not continue the head.
        let cache = temp.path().join("state.schedule.json");
        std::fs::remove_file(&cache).unwrap();
        let fourth = v4_summary(8640, "relative-v1", None);
        let error = run_on(&store, &fourth, &[]).unwrap_err().to_string();
        assert!(error.contains("no longer holds incentive-2"), "{error}");
        // Nor does a history whose incentive-2 is another record under the same id.
        std::fs::remove_file(&cache).unwrap_or_default();
        let mut rewritten = history.clone();
        rewritten[1].effective_epoch += 1;
        rewritten[1]
            .sign(|bytes| dave.sign(bytes).0.to_vec())
            .unwrap();
        let error = run_on(
            &store,
            &v4_summary(8640, "relative-v1-payout", Some("incentive-2")),
            &rewritten,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no longer holds incentive-2"), "{error}");
    }

    /// A v4 summary's verifier rows reach the burn writer with whether the chain still names
    /// them, on the first submission and on a retry; a stale one is logged and still lets
    /// the miners be weighted.
    #[test]
    fn v4_verifier_rows_reach_the_burner_checked_against_the_chain() {
        // The fixture at epoch 22, where the two-record history's payout switch is in force.
        let dave = crate::schedule::tests::key("//Dave");
        let history =
            serde_json::to_vec(&crate::schedule::tests::two_records(&dave, &dave, "local"))
                .unwrap();
        let summary = v4_summary(7920, "relative-v1-payout", Some("incentive-2"));
        let body = summary.canonical_json().unwrap().into_bytes();
        let v4 = move |_: &str, _: Duration, resource: Resource, _: Option<&str>| {
            if resource == Resource::ScheduleHistory {
                return Ok(history.clone());
            }
            Ok(body.clone())
        };
        for holds in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let mut snapshot = chain_at(&summary).snapshot;
            if holds {
                snapshot.hotkeys[90] = "5CiPPseXPECbkjWCa6MnjNokrgYjMqmKndv2rSnekmSK2DjL".into();
            }
            let burner = FakeBurner::new(&snapshot.hotkeys[12], true);
            let chain = FixedChain {
                snapshot,
                netuids: RefCell::new(vec![]),
            };
            let store = StateStore::new(temp.path().join("state.json"));
            let mut arguments = run(&chain, &store, &burner);
            arguments.fetch = &v4;
            let log = Log::default();
            assert!(log.capture(|| run_once(&arguments)).is_err());
            burner.fail.set(false);
            run_once(&arguments).unwrap();
            let calls = burner.verifiers.borrow();
            assert_eq!(calls.len(), 2);
            for call in calls.iter() {
                assert_eq!(call.len(), 1);
                assert_eq!(
                    (call[0].verifier.uid, call[0].verifier.verdicts.get()),
                    (90, 1_020)
                );
                assert_eq!(call[0].on_chain, holds);
            }
            assert_eq!(burner.scores.borrow()[1].len(), 3);
            assert_eq!(
                log.messages()
                    .iter()
                    .any(|line| line.starts_with("Verifier not paid: UID 90")),
                !holds
            );
        }
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
        assert_eq!(
            *burner.burns.borrow(),
            [BurnPolicy::Target {
                miner_target_usd_cents: 10_000,
                tao_price_usd_cents: 30_160,
            }]
        );
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

    /// A weight set 40 blocks before the finalized head under a 100-block limit is skipped
    /// without touching the burner or the state, so the next poll retries it.
    #[test]
    fn rate_limited_weights_wait_for_the_next_poll() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = fixture_snapshot();
        snapshot.last_updates[12] = snapshot.finalized_block - 40;
        snapshot.weights_rate_limit = 100;
        let burner = FakeBurner::new(&snapshot.hotkeys[12], false);
        let mut chain = FixedChain {
            snapshot,
            netuids: RefCell::new(vec![]),
        };
        let store = StateStore::new(temp.path().join("state.json"));
        let log = Log::default();
        let records = log
            .capture(|| run_once(&run(&chain, &store, &burner)))
            .unwrap();
        assert_eq!(records.len(), 3);
        assert!(burner.calls.borrow().is_empty());
        assert_eq!(store.load().unwrap().burn_epoch_end_block, None);
        assert_eq!(
            log.messages().last().unwrap(),
            "Weights rate-limited last_update=682 limit=100 allowed_from_block=782 finalized_block=722"
        );

        // Once the head passes the allowed block, the same summary is submitted.
        chain.snapshot.finalized_block = 782;
        let records = run_once(&run(&chain, &store, &burner)).unwrap();
        assert!(records.is_empty());
        assert_eq!(*burner.calls.borrow(), [(46, 782)]);
        assert_eq!(store.load().unwrap().burn_epoch_end_block, Some(720));
    }

    #[test]
    fn rate_limit_is_inert_at_zero_and_for_a_validator_that_never_set_weights() {
        let mut snapshot = fixture_snapshot();
        assert_eq!(rate_limited(&snapshot, 12), None);
        snapshot.weights_rate_limit = 100;
        assert_eq!(rate_limited(&snapshot, 12), None, "never set weights");
        snapshot.last_updates[12] = snapshot.finalized_block;
        assert_eq!(
            rate_limited(&snapshot, 12),
            Some(snapshot.finalized_block + 100)
        );
        snapshot.weights_rate_limit = 0;
        assert_eq!(rate_limited(&snapshot, 12), None, "limit 0");
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
            let payload = summary.signing_payload().unwrap();
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
        let changed = |_: &str, _: Duration, _: Resource, _: Option<&str>| {
            Ok(summary.canonical_json().unwrap().into_bytes())
        };
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
        let fetch = |_: &str, _: Duration, _: Resource, _: Option<&str>| {
            Ok(summary.canonical_json().unwrap().into_bytes())
        };
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
        assert_eq!(weights(&first), [65_535, 30_287, 54_409]);
        assert_eq!(first[2].score_bps, 8_294);
        assert!(!first[2].disqualified);
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

    /// Rewrite the log lines of `tests/golden/runtime_cases.json` from the current code,
    /// keeping every other field. Run after a scoring change with
    /// `cargo test regenerate_runtime_cases -- --ignored`, then review the diff.
    #[test]
    #[ignore = "rewrites the golden file"]
    fn regenerate_runtime_cases() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/runtime_cases.json"
        );
        let mut cases: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for case in cases.as_array_mut().unwrap() {
            let temp = tempfile::tempdir().unwrap();
            let store = StateStore::new(temp.path().join("state.json"));
            for run_case in case["runs"].as_array_mut().unwrap() {
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
                let _ = log.capture(|| run_once(&run(&chain, &store, &burner)));
                run_case["messages"] = json!(log.messages());
            }
        }
        std::fs::write(path, serde_json::to_string(&cases).unwrap() + "\n").unwrap();
    }
}
