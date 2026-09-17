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
        burn: BurnFraction,
    ) -> Result<String, BurnError>;
}

/// The share of emission sent to the owner UID, in basis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnFraction(u128);

impl BurnFraction {
    pub const NONE: Self = Self(0);
    pub const FULL: Self = Self(BPS);
    /// The share every validator applies; changing it is a release, so validators keep
    /// agreeing on weights. Use FULL for 100%, Self(5_000) for 50%, or NONE for 0%.
    /// At 0%, validators still submit the scored miner weights.
    pub const CURRENT: Self = Self::FULL;

    pub fn from_bps(bps: u128) -> Result<Self, BurnError> {
        if bps > BPS {
            return refuse("Burn fraction must be between 0.0 and 1.0");
        }
        Ok(Self(bps))
    }
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
        burn: BurnFraction,
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
        let (dests, weights) = weights(miners, uid, burn)?;

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
        burn: BurnFraction,
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
    fn the_fraction_is_bounded_and_currently_full() {
        assert_eq!(BurnFraction::from_bps(0).unwrap(), BurnFraction::NONE);
        assert_eq!(BurnFraction::from_bps(2_500).unwrap(), BurnFraction(2_500));
        assert_eq!(BurnFraction::from_bps(BPS).unwrap(), BurnFraction::FULL);
        assert_eq!(
            BurnFraction::from_bps(BPS + 1).unwrap_err().0,
            "Burn fraction must be between 0.0 and 1.0"
        );
        assert_eq!(BurnFraction::CURRENT, BurnFraction::FULL);
    }
}
