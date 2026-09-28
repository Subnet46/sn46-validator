//! The only chain write: this epoch's weights, with the burn share on the subnet owner UID.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value as Json;
use subtensor::api::{
    self,
    runtime_types::{
        bounded_collections::bounded_vec::BoundedVec,
        pallet_subtensor::pallet::RecycleOrBurnEnum,
        subtensor_runtime_common::{MechId, NetUid},
    },
};
use subxt::SubstrateConfig;
use subxt::config::DefaultExtrinsicParamsBuilder;
use subxt::config::RpcConfigFor;
use subxt::dynamic::{self, Value};
use subxt::rpcs::methods::legacy::LegacyRpcMethods;
use subxt::utils::AccountId32;
use subxt_signer::sr25519::Keypair;

use crate::chain::{At, BittensorChain};
use sn46_shared::identity::{SS58_FORMAT, encode_ss58};
use sn46_shared::scoring::{BPS, MAX_WEIGHT, MinerScore};

/// The authorized burn vector could not be submitted safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BurnError(pub String);

/// What `run_once` needs from the burn writer; `BittensorBurnWriter` is the one real
/// implementation and the ported runtime tests substitute a recording fake.
pub trait Burner {
    /// The validator hotkey's SS58 address (`wallet.hotkey.ss58_address`).
    fn hotkey(&self) -> &str;
    /// Submit this epoch's weights pinned to `finalized_block`; the returned message is
    /// logged.
    fn submit(
        &self,
        netuid: u64,
        finalized_block: u64,
        miners: &[MinerScore],
        burn: BurnPolicy,
    ) -> Result<String, BurnError>;
}

/// The share of emission sent to the owner UID, in basis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnFraction(u128);

impl BurnFraction {
    pub const NONE: Self = Self(0);
    pub const FULL: Self = Self(BPS);

    pub fn from_bps(bps: u128) -> Result<Self, BurnError> {
        if bps > BPS {
            return refuse("Burn fraction must be between 0.0 and 1.0");
        }
        Ok(Self(bps))
    }
}

/// How the burn fraction is chosen for one submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnPolicy {
    Fixed(BurnFraction),
    /// Burn whatever leaves the miners `miner_target_usd_cents` per epoch, priced from the
    /// chain at the submission's finalized block and the summary's signed TAO price.
    Target {
        miner_target_usd_cents: u64,
        tao_price_usd_cents: u64,
    },
}

/// Whole percents: validators reading the chain a few blocks apart see slightly different
/// prices, and rounding down to 1% lands them on the same fraction nearly always.
const BURN_STEP_BPS: u128 = 100;
const RAO_PER_TAO: u128 = 1_000_000_000;
const U16_MAX: u128 = u16::MAX as u128;

/// The chain inputs that price one epoch of miner emission, read at one finalized block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochEconomics {
    /// Alpha rao emitted to the subnet's participants per block.
    pub alpha_out_emission: u64,
    pub tempo: u16,
    /// The owner's share of emission, out of `u16::MAX`.
    pub owner_cut: u16,
    /// The pool's TAO and alpha reserves, in rao; their ratio is the spot price.
    pub subnet_tao: u64,
    pub subnet_alpha_in: u64,
}

impl EpochEconomics {
    async fn read(at: &At, netuid: u16) -> Result<Self, BurnError> {
        let storage = at.storage();
        let per_subnet = async |name: &str| -> Result<u64, BurnError> {
            storage
                .fetch(
                    dynamic::storage::<(u16,), u64>("SubtensorModule", name),
                    (netuid,),
                )
                .await
                .map_err(lookup)?
                .decode()
                .map_err(lookup)
        };
        let alpha_out_emission = per_subnet("SubnetAlphaOutEmission").await?;
        let subnet_tao = per_subnet("SubnetTAO").await?;
        let subnet_alpha_in = per_subnet("SubnetAlphaIn").await?;
        let owner_cut = storage
            .fetch(
                dynamic::storage::<(), u16>("SubtensorModule", "SubnetOwnerCut"),
                (),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        let tempo = crate::chain::read_tempo(at, netuid).await.map_err(lookup)?;
        Ok(Self {
            alpha_out_emission,
            tempo,
            owner_cut,
            subnet_tao,
            subnet_alpha_in,
        })
    }

    /// The miners' epoch emission (after the owner cut, half of the rest) valued in TAO rao.
    pub fn miners_tao_rao(&self) -> Result<u128, BurnError> {
        if self.subnet_alpha_in == 0 {
            return refuse("Subnet pool holds no alpha; its price is undefined");
        }
        let miners_alpha = u128::from(self.alpha_out_emission)
            * u128::from(self.tempo)
            * (U16_MAX - u128::from(self.owner_cut))
            / (2 * U16_MAX);
        miners_alpha
            .checked_mul(u128::from(self.subnet_tao))
            .map(|value| value / u128::from(self.subnet_alpha_in))
            .ok_or_else(overflow)
    }

    /// The largest whole-percent burn that still leaves the miners at least the target;
    /// NONE when their whole share is worth no more than it.
    ///
    /// Zero chain values, as a fresh localnet may hold them: no `alpha_out_emission` or
    /// no `subnet_tao` values the miners' share at nothing, which is at most any target,
    /// so the burn is NONE and every scored miner keeps its weight; no `subnet_alpha_in`
    /// leaves the price undefined, which is an error, so no weights are set that run and
    /// the next poll retries.
    pub fn burn_for_target(
        &self,
        miner_target_usd_cents: u64,
        tao_price_usd_cents: u64,
    ) -> Result<BurnFraction, BurnError> {
        let miners_value = self
            .miners_tao_rao()?
            .checked_mul(u128::from(tao_price_usd_cents))
            .ok_or_else(overflow)?;
        let target_value = u128::from(miner_target_usd_cents) * RAO_PER_TAO;
        if miners_value <= target_value {
            return Ok(BurnFraction::NONE);
        }
        let keep = (BPS * target_value).div_ceil(miners_value);
        let burn = BPS - keep;
        Ok(BurnFraction(burn - burn % BURN_STEP_BPS))
    }
}

fn overflow() -> BurnError {
    BurnError("Miner emission value overflows".into())
}

/// This epoch's weight vector: every scored miner scaled by `1 - burn`, the freed share on
/// the owner UID, max-upscaled to `MAX_WEIGHT`. `(dests, weights)` in UID order, zeros
/// dropped; a full burn, or nothing scored, is the owner alone.
pub fn weights(
    miners: &[MinerScore],
    owner_uid: u16,
    burn: BurnFraction,
) -> Result<(Vec<u16>, Vec<u16>), BurnError> {
    let total: u128 = miners.iter().map(|record| record.normalized_weight).sum();
    if burn == BurnFraction::FULL || total == 0 {
        return Ok((vec![owner_uid], vec![MAX_WEIGHT as u16]));
    }
    let mut shares = BTreeMap::new();
    for record in miners {
        let uid = u16::try_from(record.miner.uid)
            .map_err(|_| BurnError(format!("Miner UID {} is not a chain UID", record.miner.uid)))?;
        *shares.entry(uid).or_insert(0) += record.normalized_weight * (BPS - burn.0);
    }
    *shares.entry(owner_uid).or_insert(0) += total * burn.0;
    let highest = shares.values().copied().max().unwrap_or(0);
    Ok(shares
        .into_iter()
        .map(|(uid, share)| (uid, ((share * MAX_WEIGHT + highest / 2) / highest) as u16))
        .filter(|(_, weight)| *weight > 0)
        .unzip())
}

/// The one line that records what goes on chain: the owner UID, the burn in basis points
/// and the `(dests, weights)` vector as submitted.
pub fn weights_line(owner_uid: u16, burn: BurnFraction, dests: &[u16], weights: &[u16]) -> String {
    format!(
        "Weights prepared owner_uid={owner_uid} burn_bps={} dests={dests:?} weights={weights:?}",
        burn.0
    )
}

fn refuse<T>(message: impl Into<String>) -> Result<T, BurnError> {
    Err(BurnError(message.into()))
}

fn lookup(error: impl std::fmt::Display) -> BurnError {
    BurnError(format!("Burn pre-check failed: {error}"))
}

const MORTALITY_BLOCKS: u64 = 64;

/// Written before broadcasting. Until this era expires, a failed watch must be
/// reconciled through finalized LastUpdate, never retried with a new nonce.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingSubmission {
    genesis: String,
    netuid: u16,
    hotkey: String,
    expires_at: u64,
}

/// The subnet state a burn is checked against, read at one finalized block.
struct SubnetBurnState {
    mode: RecycleOrBurnEnum,
    commit_reveal: bool,
    /// `None` when the chain holds only the storage default, the zero account.
    owner: Option<AccountId32>,
    /// `None` when the owner hotkey has no UID on the subnet.
    owner_uid: Option<u16>,
    version_key: u64,
}

impl SubnetBurnState {
    async fn read(at: &At, netuid: u16) -> Result<Self, BurnError> {
        let storage = at.storage();
        let mode = storage
            .fetch(
                dynamic::storage::<(u16,), RecycleOrBurnEnum>("SubtensorModule", "RecycleOrBurn"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        let commit_reveal = storage
            .fetch(
                dynamic::storage::<(u16,), bool>("SubtensorModule", "CommitRevealWeightsEnabled"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        let owner: AccountId32 = storage
            .fetch(
                dynamic::storage::<(u16,), AccountId32>("SubtensorModule", "SubnetOwnerHotkey"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        let owner = (owner.0 != [0; 32]).then_some(owner);
        let owner_uid = match &owner {
            Some(owner) => storage
                .try_fetch(
                    dynamic::storage::<(u16, AccountId32), u16>("SubtensorModule", "Uids"),
                    (netuid, *owner),
                )
                .await
                .map_err(lookup)?
                .map(|uid| uid.decode())
                .transpose()
                .map_err(lookup)?,
            None => None,
        };
        let version_key = storage
            .fetch(
                dynamic::storage::<(u16,), u64>("SubtensorModule", "WeightsVersionKey"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        Ok(Self {
            mode,
            commit_reveal,
            owner,
            owner_uid,
            version_key,
        })
    }

    /// The owner UID and weights version the burn is signed with, or the first refusal.
    fn check(&self) -> Result<(u16, u64), BurnError> {
        if !matches!(self.mode, RecycleOrBurnEnum::Burn) {
            return refuse("Subnet is not in Burn mode");
        }
        if self.owner.is_none() {
            return refuse("Subnet owner hotkey is invalid");
        }
        let Some(uid) = self.owner_uid else {
            return refuse("Subnet owner UID or weights version is invalid");
        };
        Ok((uid, self.version_key))
    }
}

/// Signs with the `bittensor_wallet` hotkey file at `<path>/<name>/hotkeys/<hotkey>` and
/// writes through the chain reader's connection.
pub struct BittensorBurnWriter<'a> {
    chain: &'a BittensorChain,
    keypair: Keypair,
    hotkey: String,
    pending_path: PathBuf,
    timeout: Duration,
}

impl<'a> BittensorBurnWriter<'a> {
    pub fn new(
        chain: &'a BittensorChain,
        wallet_name: &str,
        wallet_hotkey: &str,
        wallet_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
    ) -> Result<Self, BurnError> {
        let path = wallet_path
            .as_ref()
            .join(wallet_name)
            .join("hotkeys")
            .join(wallet_hotkey);
        let raw = fs::read(&path).map_err(|error| {
            BurnError(format!(
                "Validator hotkey file {} is unreadable: {error}",
                path.display()
            ))
        })?;
        if raw.starts_with(b"$NACL") {
            return refuse(
                "Validator hotkey file is encrypted; the validator needs an unencrypted hotkey",
            );
        }
        let file: Json = serde_json::from_slice(&raw)
            .map_err(|_| BurnError("Validator hotkey file is not JSON".into()))?;
        let seed: [u8; 32] = file["secretSeed"]
            .as_str()
            .and_then(|text| hex::decode(text.trim_start_matches("0x")).ok())
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| BurnError("Validator hotkey file has no 32-byte secretSeed".into()))?;
        let keypair = Keypair::from_secret_key(seed)
            .map_err(|error| BurnError(format!("Validator hotkey seed is invalid: {error}")))?;
        let hotkey = encode_ss58(&keypair.public_key().0, SS58_FORMAT);
        if file["ss58Address"].as_str() != Some(hotkey.as_str()) {
            return refuse("Validator hotkey file ss58Address does not match its secretSeed");
        }
        Ok(Self {
            chain,
            keypair,
            hotkey,
            pending_path: state_path.as_ref().with_added_extension("submission.json"),
            timeout: Duration::from_secs(60),
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn save_pending(&self, pending: Option<&PendingSubmission>) -> Result<(), BurnError> {
        crate::state::save_bytes(
            &self.pending_path,
            &serde_json::to_vec(&pending).map_err(lookup)?,
        )
        .map_err(lookup)
    }

    fn check_pending(&self, netuid: u16, block: u64) -> Result<(), BurnError> {
        let pending: Option<PendingSubmission> = match fs::read(&self.pending_path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(lookup)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(lookup(error)),
        };
        if let Some(pending) = pending {
            if pending.genesis != format!("{:?}", self.chain.client().genesis_hash())
                || pending.netuid != netuid
                || pending.hotkey != self.hotkey
            {
                return refuse(
                    "Pending submission belongs to a different chain, subnet, or hotkey",
                );
            }
            if block < pending.expires_at {
                return refuse(format!(
                    "Previous submission is uncertain until finalized block {}; reconcile LastUpdate before retrying",
                    pending.expires_at
                ));
            }
        }
        Ok(())
    }

    async fn submit_at(
        &self,
        netuid: u64,
        finalized_block: u64,
        miners: &[MinerScore],
        burn: BurnPolicy,
    ) -> Result<String, BurnError> {
        let at = self
            .chain
            .client()
            .at_block(finalized_block)
            .await
            .map_err(lookup)?;
        let netuid = u16::try_from(netuid).map_err(lookup)?;
        self.check_pending(netuid, finalized_block)?;
        let subnet = SubnetBurnState::read(&at, netuid).await?;
        let (uid, version) = subnet.check()?;
        let burn = match burn {
            BurnPolicy::Fixed(burn) => burn,
            BurnPolicy::Target {
                miner_target_usd_cents,
                tao_price_usd_cents,
            } => {
                let economics = EpochEconomics::read(&at, netuid).await?;
                let burn =
                    economics.burn_for_target(miner_target_usd_cents, tao_price_usd_cents)?;
                tracing::info!(
                    "Burn priced burn_bps={} miners_tao_rao={} miner_target_usd_cents={miner_target_usd_cents} tao_price_usd_cents={tao_price_usd_cents} alpha_out_emission={} tempo={} owner_cut={} subnet_tao={} subnet_alpha_in={}",
                    burn.0,
                    economics.miners_tao_rao()?,
                    economics.alpha_out_emission,
                    economics.tempo,
                    economics.owner_cut,
                    economics.subnet_tao,
                    economics.subnet_alpha_in,
                );
                burn
            }
        };
        let (dests, weights) = weights(miners, uid, burn)?;
        tracing::info!("{}", weights_line(uid, burn, &dests, &weights));

        // Use a pool-aware nonce, but a mortal era so an uncertain transaction cannot
        // execute indefinitely after our watch times out.
        let methods =
            LegacyRpcMethods::<RpcConfigFor<SubstrateConfig>>::new(self.chain.rpc().clone());
        let nonce = methods
            .system_account_next_index(&self.keypair.public_key().to_account_id())
            .await
            .map_err(|error| BurnError(format!("Burn write raised: {error}")))?;
        let params = DefaultExtrinsicParamsBuilder::<SubstrateConfig>::new()
            .mortal_from_unchecked(MORTALITY_BLOCKS, finalized_block, at.block_hash())
            .nonce(nonce)
            .build();
        // Offline signable: subxt's online path would first query AccountNonceApi at the
        // pinned block, which is the stale nonce the SDK avoids.
        let raised =
            |error: &dyn std::fmt::Display| BurnError(format!("Burn write raised: {error}"));
        let signed = if subnet.commit_reveal {
            // Schedule against the best head, as the SDK does, so finalized-head lag
            // does not put an epoch-boundary submission in the wrong reveal window.
            let head = methods
                .chain_get_block_hash(None)
                .await
                .map_err(lookup)?
                .ok_or_else(|| lookup("best block unavailable"))?;
            let head = self.chain.client().at_block(head).await.map_err(lookup)?;
            let (commit, round) = crate::commit_reveal::encrypted_weights(
                &head,
                netuid,
                &self.keypair.public_key().0,
                dests,
                weights,
                version,
            )
            .await?;
            let call = api::tx()
                .subtensor_module()
                .commit_timelocked_mechanism_weights(
                    NetUid(netuid),
                    MechId(0),
                    BoundedVec(commit),
                    round,
                    4,
                );
            at.tx()
                .create_signable_offline(&call, params)
                .map_err(|error| raised(&error))?
                .sign(&self.keypair)
                .map_err(|error| raised(&error))?
        } else {
            // Encode with live metadata: older localnets use primitive netuid/mecid
            // types rather than the newtypes in the generated Finney interface.
            let call = dynamic::tx(
                "SubtensorModule",
                "set_mechanism_weights",
                vec![
                    Value::u128(u128::from(netuid)),
                    Value::u128(0),
                    Value::unnamed_composite(
                        dests.into_iter().map(|uid| Value::u128(u128::from(uid))),
                    ),
                    Value::unnamed_composite(
                        weights
                            .into_iter()
                            .map(|weight| Value::u128(u128::from(weight))),
                    ),
                    Value::u128(u128::from(version)),
                ],
            );
            at.tx()
                .create_signable_offline(&call, params)
                .map_err(|error| raised(&error))?
                .sign(&self.keypair)
                .map_err(|error| raised(&error))?
        };
        self.save_pending(Some(&PendingSubmission {
            genesis: format!("{:?}", self.chain.client().genesis_hash()),
            netuid,
            hotkey: self.hotkey.clone(),
            expires_at: finalized_block
                .checked_add(MORTALITY_BLOCKS)
                .ok_or_else(|| lookup("block overflow"))?,
        }))?;
        let failed =
            |error: &dyn std::fmt::Display| BurnError(format!("Burn write failed: {error}"));
        let finalized = signed
            .submit_and_watch()
            .await
            // Debug retains the RPC's custom transaction error code; Display drops it.
            .map_err(|error| BurnError(format!("Burn write failed: {error:?}")))?
            .wait_for_finalized()
            .await
            .map_err(|error| failed(&error))?;
        // Finalization alone is not success, and subxt only rejects ExtrinsicFailed: the
        // block's events must carry ExtrinsicSuccess for this extrinsic.
        let events = finalized
            .wait_for_success()
            .await
            .map_err(|error| failed(&error))?;
        let succeeded = events.iter().any(|event| {
            event.is_ok_and(|event| {
                event.pallet_name() == "System" && event.event_name() == "ExtrinsicSuccess"
            })
        });
        if !succeeded {
            return Err(failed(&"finalized without an ExtrinsicSuccess event"));
        }
        self.save_pending(None)?;
        Ok(if subnet.commit_reveal {
            "commit finalized; chain will reveal weights"
        } else {
            "finalized"
        }
        .into())
    }
}

impl Burner for BittensorBurnWriter<'_> {
    fn hotkey(&self) -> &str {
        &self.hotkey
    }

    fn submit(
        &self,
        netuid: u64,
        finalized_block: u64,
        miners: &[MinerScore],
        burn: BurnPolicy,
    ) -> Result<String, BurnError> {
        self.chain.block_on(async {
            tokio::time::timeout(self.timeout, self.submit_at(netuid, finalized_block, miners, burn))
                .await.map_err(|_| BurnError("Burn submission timed out; outcome may be uncertain, reconcile before retrying".into()))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::tests::fixture;
    use sn46_shared::scoring::score_epoch_summary;

    const OWNER: u16 = 238;

    fn scored() -> Vec<MinerScore> {
        score_epoch_summary(&fixture())
    }

    fn weight_of(dests: &[u16], values: &[u16], uid: u16) -> u16 {
        values[dests.iter().position(|dest| *dest == uid).unwrap()]
    }

    #[test]
    fn full_burn_or_nothing_scored_is_the_owner_alone() {
        let owner_alone = (vec![OWNER], vec![65_535]);
        assert_eq!(
            weights(&scored(), OWNER, BurnFraction::FULL).unwrap(),
            owner_alone
        );
        assert_eq!(
            weights(&[], OWNER, BurnFraction::NONE).unwrap(),
            owner_alone
        );
    }

    #[test]
    fn the_weights_line_shows_the_vector_as_submitted() {
        let (dests, weights) = weights(&scored(), OWNER, BurnFraction(5_000)).unwrap();
        assert_eq!(
            weights_line(OWNER, BurnFraction(5_000), &dests, &weights),
            format!(
                "Weights prepared owner_uid=238 burn_bps=5000 dests={dests:?} weights={weights:?}"
            )
        );
        assert_eq!(
            weights_line(3, BurnFraction::FULL, &[3], &[65_535]),
            "Weights prepared owner_uid=3 burn_bps=10000 dests=[3] weights=[65535]"
        );
    }

    #[test]
    fn zero_burn_with_one_scored_miner_gives_it_the_whole_weight() {
        let records: Vec<_> = scored()
            .into_iter()
            .filter(|record| record.normalized_weight > 0)
            .take(1)
            .collect();
        let uid = records[0].miner.uid as u16;
        assert_ne!(uid, OWNER);
        assert_eq!(
            weights(&records, OWNER, BurnFraction::NONE).unwrap(),
            (vec![uid], vec![65_535])
        );
    }

    #[test]
    fn zero_burn_is_the_scored_vector() {
        let records = scored();
        let (dests, values) = weights(&records, OWNER, BurnFraction::NONE).unwrap();
        let expected: Vec<(u16, u16)> = records
            .iter()
            .filter(|record| record.normalized_weight > 0)
            .map(|record| (record.miner.uid as u16, record.normalized_weight as u16))
            .collect();
        assert!(expected.len() > 1, "{expected:?}");
        assert_eq!(dests.into_iter().zip(values).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn half_burn_gives_the_owner_the_miners_total() {
        let records = scored();
        let total: u128 = records.iter().map(|record| record.normalized_weight).sum();
        let (dests, values) = weights(&records, OWNER, BurnFraction(5_000)).unwrap();
        assert_eq!(weight_of(&dests, &values, OWNER), 65_535);
        for record in records.iter().filter(|record| record.normalized_weight > 0) {
            assert_eq!(
                u128::from(weight_of(&dests, &values, record.miner.uid as u16)),
                (record.normalized_weight * MAX_WEIGHT + total / 2) / total,
                "uid {}",
                record.miner.uid
            );
        }
    }

    #[test]
    fn an_owner_that_mines_holds_both_shares_once() {
        let records = scored();
        let top = records
            .iter()
            .max_by_key(|record| record.normalized_weight)
            .unwrap();
        let owner = top.miner.uid as u16;
        let (dests, values) = weights(&records, owner, BurnFraction(5_000)).unwrap();
        assert_eq!(dests.iter().filter(|dest| **dest == owner).count(), 1);
        assert!(dests.is_sorted_by(|left, right| left < right));
        assert_eq!(weight_of(&dests, &values, owner), 65_535);
    }

    #[test]
    fn the_fraction_is_bounded() {
        assert_eq!(BurnFraction::from_bps(0).unwrap(), BurnFraction::NONE);
        assert_eq!(BurnFraction::from_bps(2_500).unwrap(), BurnFraction(2_500));
        assert_eq!(BurnFraction::from_bps(BPS).unwrap(), BurnFraction::FULL);
        assert_eq!(
            BurnFraction::from_bps(BPS + 1).unwrap_err().0,
            "Burn fraction must be between 0.0 and 1.0"
        );
    }

    /// Subnet 46 on Finney, 2026-09-28: 1 alpha per block, tempo 360, an 18% owner cut and
    /// 0.00358 TAO per alpha.
    const FINNEY_46: EpochEconomics = EpochEconomics {
        alpha_out_emission: 1_000_000_000,
        tempo: 360,
        owner_cut: 11_796,
        subnet_tao: 8_515_450_000_000,
        subnet_alpha_in: 2_378_997_012_605_306,
    };

    /// What the miners receive under `burn`, in US cents.
    fn miners_usd_cents(economics: &EpochEconomics, burn: BurnFraction, tao_price: u64) -> u128 {
        economics.miners_tao_rao().unwrap() * u128::from(tao_price) * (BPS - burn.0)
            / BPS
            / RAO_PER_TAO
    }

    #[test]
    fn a_hundred_dollar_target_burns_37_percent_on_finney_46() {
        // 147.6 alpha to the miners, worth 0.528 TAO, $159.34 at $301.60 per TAO.
        assert_eq!(FINNEY_46.miners_tao_rao().unwrap(), 528_326_614);
        let burn = FINNEY_46.burn_for_target(10_000, 30_160).unwrap();
        assert_eq!(burn, BurnFraction(3_700));
        assert_eq!(miners_usd_cents(&FINNEY_46, burn, 30_160), 10_038);
    }

    #[test]
    fn the_burn_rounds_down_so_the_miners_never_fall_short() {
        for target in [1, 999, 5_000, 10_000, 12_345, 15_933] {
            for price in [10_000, 30_160, 45_001] {
                let Ok(burn) = FINNEY_46.burn_for_target(target, price) else {
                    panic!("{target} at {price}");
                };
                assert_eq!(burn.0 % BURN_STEP_BPS, 0);
                // Exact, in rao-cents scaled by BPS: kept at least the target, and one more
                // step would have left the miners short.
                let value = FINNEY_46.miners_tao_rao().unwrap() * u128::from(price);
                let target = u128::from(target) * RAO_PER_TAO * BPS;
                if value * BPS <= target {
                    assert_eq!(burn, BurnFraction::NONE, "{target} at {price}");
                    continue;
                }
                assert!(value * (BPS - burn.0) >= target, "{burn:?} at {price}");
                if burn.0 + BURN_STEP_BPS <= BPS {
                    assert!(
                        value * (BPS - burn.0 - BURN_STEP_BPS) < target,
                        "{burn:?} at {price}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_share_worth_no_more_than_the_target_is_not_burned() {
        // $159.34 of miner emission against a $200 target, or an exact match.
        assert_eq!(
            FINNEY_46.burn_for_target(20_000, 30_160).unwrap(),
            BurnFraction::NONE
        );
        let no_emission = EpochEconomics {
            alpha_out_emission: 0,
            ..FINNEY_46
        };
        assert_eq!(
            no_emission.burn_for_target(10_000, 30_160).unwrap(),
            BurnFraction::NONE
        );
        let no_tao = EpochEconomics {
            subnet_tao: 0,
            ..FINNEY_46
        };
        assert_eq!(
            no_tao.burn_for_target(10_000, 30_160).unwrap(),
            BurnFraction::NONE
        );
    }

    #[test]
    fn an_empty_pool_cannot_price_the_burn() {
        let empty = EpochEconomics {
            subnet_alpha_in: 0,
            ..FINNEY_46
        };
        assert_eq!(
            empty.burn_for_target(10_000, 30_160).unwrap_err().0,
            "Subnet pool holds no alpha; its price is undefined"
        );
    }

    #[test]
    fn the_owner_cut_and_extreme_reserves_price_without_overflow() {
        let no_cut = EpochEconomics {
            owner_cut: 0,
            ..FINNEY_46
        };
        // Half of the emission rather than 41% of it: $194.32.
        assert_eq!(
            miners_usd_cents(&no_cut, BurnFraction::NONE, 30_160),
            19_432
        );
        assert_eq!(
            no_cut.burn_for_target(10_000, 30_160).unwrap(),
            BurnFraction(4_800)
        );
        // A thousand times today's emission, reserves and TAO price still price: $159 billion
        // of emission against a $100 target burns all but a sliver, rounded down to 99%.
        let large = EpochEconomics {
            alpha_out_emission: 1_000 * FINNEY_46.alpha_out_emission,
            subnet_tao: 1_000 * FINNEY_46.subnet_tao,
            ..FINNEY_46
        };
        assert_eq!(
            large.burn_for_target(10_000, 30_160_000).unwrap(),
            BurnFraction(9_900)
        );
        let extreme = EpochEconomics {
            alpha_out_emission: u64::MAX,
            tempo: u16::MAX,
            owner_cut: 0,
            subnet_tao: u64::MAX,
            subnet_alpha_in: 1,
        };
        assert_eq!(
            extreme.burn_for_target(10_000, 30_160).unwrap_err().0,
            "Miner emission value overflows"
        );
    }

    /// Reads subnet 46's live economics from Finney and prices a $100 target at $300 per
    /// TAO: `cargo test finney_46_prices_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn finney_46_prices_live() {
        let chain =
            BittensorChain::connect("finney", "wss://entrypoint-finney.opentensor.ai:443").unwrap();
        let economics = chain
            .block_on(async {
                let at = chain.client().at_current_block().await.map_err(lookup)?;
                EpochEconomics::read(&at, 46).await
            })
            .unwrap();
        let burn = economics.burn_for_target(10_000, 30_000).unwrap();
        println!("{economics:?} burn={burn:?}");
        assert_eq!(economics.tempo, 360);
        assert!(economics.subnet_alpha_in > 0);
    }
}
