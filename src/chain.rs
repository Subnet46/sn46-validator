use std::collections::HashMap;
use std::sync::Arc;
use subxt::backend::LegacyBackend;
use subxt::client::{ClientAtBlock, OnlineClientAtBlockImpl};
use subxt::dynamic;
use subxt::rpcs::RpcClient;
use subxt::rpcs::client::jsonrpsee_client;
use subxt::utils::AccountId32;
use subxt::{OnlineClient, SubstrateConfig};

use sn46_shared::epoch_summary::EpochSummary;
use sn46_shared::identity::{SS58_FORMAT, encode_ss58};

/// The SDK's network table (`bittensor.core.settings.NETWORK_MAP`) with the genesis hash
/// each public network must answer with; `CHAIN_ENDPOINT` overrides the URL only.
const NETWORKS: [(&str, &str, Option<&str>); 3] = [
    (
        "finney",
        "wss://entrypoint-finney.opentensor.ai:443",
        Some("0x2f0555cc76fc2840a25a6ea3b9637146806f1f44b090c175ffde2a7e5ab36c03"),
    ),
    (
        "test",
        "wss://test.finney.opentensor.ai:443",
        Some("0x8f9cf856bf558a14440e75569c9e58594757048d7b3a84b5d25f6bd978263105"),
    ),
    ("local", "ws://127.0.0.1:9944", None),
];

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct ChainSnapshot {
    pub finalized_block: u64,
    pub tempo: u64,
    pub last_step: u64,
    pub hotkeys: Vec<String>,
    pub last_updates: Vec<u64>,
}

/// Finalized chain state is unavailable or inconsistent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("unknown network: {0}")]
    UnknownNetwork(String),
    #[error("chain connection failed: {0}")]
    Connection(String),
    #[error("chain genesis hash does not match network {0}")]
    Genesis(String),
    #[error("finalized chain lookup failed: {0}")]
    Lookup(String),
    #[error("finalized subnet is empty or unknown")]
    EmptySubnet,
    #[error("finalized epoch state is inconsistent")]
    InconsistentEpoch,
    #[error("finalized hotkey roster is invalid")]
    InvalidRoster,
    #[error("summary tempo does not match finalized chain state")]
    Tempo,
    #[error("summary is not the latest finalized chain epoch")]
    StaleEpoch,
    #[error("summary finalized block is ahead of chain state")]
    BlockAhead,
    #[error("summary epoch start does not match finalized chain state")]
    EpochStart,
    #[error("summary UID/hotkey mapping is invalid for UID {0}")]
    UidHotkey(u64),
}

/// What `run_once` needs from the chain; `BittensorChain` is the one real implementation
/// and the ported runtime tests substitute a fixed snapshot.
pub trait Chain {
    fn snapshot(&self, netuid: u64) -> Result<ChainSnapshot, ChainError>;
}

pub fn validate_chain(
    epoch_summary: &EpochSummary,
    snapshot: &ChainSnapshot,
) -> Result<(), ChainError> {
    if epoch_summary.tempo.get() != snapshot.tempo {
        return Err(ChainError::Tempo);
    }
    if epoch_summary.epoch_end_block != snapshot.last_step {
        return Err(ChainError::StaleEpoch);
    }
    if epoch_summary.finalized_block > snapshot.finalized_block {
        return Err(ChainError::BlockAhead);
    }
    let expected_start = i128::from(snapshot.last_step) - i128::from(snapshot.tempo) + 1;
    if i128::from(epoch_summary.epoch_start_block) != expected_start {
        return Err(ChainError::EpochStart);
    }
    for miner in &epoch_summary.miners {
        let known = usize::try_from(miner.uid)
            .ok()
            .and_then(|index| snapshot.hotkeys.get(index))
            .is_some_and(|expected| *expected == miner.hotkey);
        if !known {
            return Err(ChainError::UidHotkey(miner.uid));
        }
    }
    Ok(())
}

fn lookup(error: impl std::fmt::Display) -> ChainError {
    ChainError::Lookup(error.to_string())
}

pub(crate) type At = ClientAtBlock<SubstrateConfig, OnlineClientAtBlockImpl<SubstrateConfig>>;

pub(crate) async fn read_tempo(at: &At, netuid: u16) -> Result<u16, ChainError> {
    // The codegen hash includes the storage default, which differs in fast-runtime.
    // Decode the u16 against live metadata instead of requiring Finney's default.
    at.storage()
        .fetch(
            dynamic::storage::<(u16,), u16>("SubtensorModule", "Tempo"),
            (netuid,),
        )
        .await
        .map_err(lookup)?
        .decode()
        .map_err(lookup)
}

/// One node connection on its own current-thread runtime; reads are pinned to the
/// finalized head through the legacy RPC backend only.
pub struct BittensorChain {
    runtime: tokio::runtime::Runtime,
    rpc: RpcClient,
    client: OnlineClient<SubstrateConfig>,
}

impl BittensorChain {
    pub fn connect(network: &str, endpoint: &str) -> Result<Self, ChainError> {
        let Some(&(_, default_url, genesis)) =
            NETWORKS.iter().find(|(name, _, _)| *name == network)
        else {
            return Err(ChainError::UnknownNetwork(network.into()));
        };
        let url = if endpoint.is_empty() {
            default_url
        } else {
            endpoint
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ChainError::Connection(error.to_string()))?;
        let (rpc, client) = runtime.block_on(async {
            let rpc = RpcClient::new(
                jsonrpsee_client(url)
                    .await
                    .map_err(|error| ChainError::Connection(error.to_string()))?,
            );
            let backend = LegacyBackend::<SubstrateConfig>::builder().build(rpc.clone());
            let client = OnlineClient::from_backend(Arc::new(backend))
                .await
                .map_err(|error| ChainError::Connection(error.to_string()))?;
            Ok::<_, ChainError>((rpc, client))
        })?;
        if let Some(genesis) = genesis
            && format!("{:?}", client.genesis_hash()) != genesis
        {
            return Err(ChainError::Genesis(network.into()));
        }
        Ok(Self {
            runtime,
            rpc,
            client,
        })
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub fn client(&self) -> &OnlineClient<SubstrateConfig> {
        &self.client
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// Read and validate one finalized snapshot; RPC and decoding failures are lookup errors.
    async fn read(&self, netuid: u64) -> Result<ChainSnapshot, ChainError> {
        let at = self.client.at_current_block().await.map_err(lookup)?;
        let storage = at.storage();
        // Runtime 393 uses primitive subnet IDs; newer runtimes wrap them in
        // newtypes. Decode against live metadata while retaining explicit types.
        let netuid = u16::try_from(netuid).map_err(lookup)?;
        let size: u16 = storage
            .fetch(
                dynamic::storage::<(u16,), u16>("SubtensorModule", "SubnetworkN"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        let tempo = read_tempo(&at, netuid).await?;
        let last_step: u64 = storage
            .fetch(
                dynamic::storage::<(u16,), u64>("SubtensorModule", "LastMechansimStepBlock"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        let blocks_since_last_step: u64 = storage
            .fetch(
                dynamic::storage::<(u16,), u64>("SubtensorModule", "BlocksSinceLastStep"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        let last_updates: Vec<u64> = storage
            .fetch(
                dynamic::storage::<(u16,), Vec<u64>>("SubtensorModule", "LastUpdate"),
                (netuid,),
            )
            .await
            .map_err(lookup)?
            .decode()
            .map_err(lookup)?;
        // The roster is paged in as one map; a UID without an entry is `KeyError: {uid}`,
        // the lowest one first.
        let mut roster = HashMap::new();
        let mut entries = storage
            .iter(
                dynamic::storage::<(u16, u16), AccountId32>("SubtensorModule", "Keys"),
                (netuid,),
            )
            .await
            .map_err(lookup)?;
        while let Some(entry) = entries.next().await {
            let entry = entry.map_err(lookup)?;
            let (_, uid) = entry.key().map_err(lookup)?.decode().map_err(lookup)?;
            let account = entry.value().decode().map_err(lookup)?;
            roster.insert(uid, account);
        }
        let hotkeys = (0..size)
            .map(|uid| match roster.get(&uid) {
                Some(account) => Ok(encode_ss58(&account.0, SS58_FORMAT)),
                None => Err(lookup(format!("KeyError: {uid}"))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if size == 0 {
            return Err(ChainError::EmptySubnet);
        }
        if tempo == 0 || last_step.checked_add(blocks_since_last_step) != Some(at.block_number()) {
            return Err(ChainError::InconsistentEpoch);
        }
        if hotkeys.len() != last_updates.len() {
            return Err(ChainError::InvalidRoster);
        }
        Ok(ChainSnapshot {
            finalized_block: at.block_number(),
            tempo: u64::from(tempo),
            last_step,
            hotkeys,
            last_updates,
        })
    }
}

impl Chain for BittensorChain {
    fn snapshot(&self, netuid: u64) -> Result<ChainSnapshot, ChainError> {
        self.block_on(self.read(netuid))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::Value;

    use super::*;

    pub(crate) fn fixture() -> EpochSummary {
        serde_json::from_str(include_str!("../tests/fixtures/epoch_summary_v2.json")).unwrap()
    }

    /// 56 slots, the fixture's miners in theirs.
    pub(crate) fn fixture_snapshot() -> ChainSnapshot {
        let epoch_summary = fixture();
        let mut hotkeys: Vec<String> = (0..56).map(|uid| format!("unused-{uid}")).collect();
        for miner in &epoch_summary.miners {
            hotkeys[miner.uid as usize] = miner.hotkey.clone();
        }
        ChainSnapshot {
            finalized_block: epoch_summary.finalized_block,
            tempo: epoch_summary.tempo.get(),
            last_step: epoch_summary.epoch_end_block,
            last_updates: vec![0; hotkeys.len()],
            hotkeys,
        }
    }

    pub(crate) fn snapshot_of(value: &Value) -> ChainSnapshot {
        serde_json::from_value(value.clone()).unwrap()
    }

    #[test]
    fn chain_must_match_epoch_summary_epoch_and_roster() {
        let epoch_summary = fixture();
        let base = fixture_snapshot();
        let invalid = [
            ChainSnapshot {
                tempo: 361,
                ..base.clone()
            },
            ChainSnapshot {
                last_step: 719,
                ..base.clone()
            },
            ChainSnapshot {
                finalized_block: 719,
                ..base.clone()
            },
            ChainSnapshot {
                hotkeys: base.hotkeys[..55].to_vec(),
                last_updates: base.last_updates[..55].to_vec(),
                ..base.clone()
            },
        ];
        assert!(validate_chain(&epoch_summary, &base).is_ok());
        for snapshot in invalid {
            assert!(
                validate_chain(&epoch_summary, &snapshot).is_err(),
                "{snapshot:?}"
            );
        }
    }

    #[test]
    fn golden_validate_chain_verdicts() {
        let cases: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/golden/chain_cases.json"
            ))
            .unwrap(),
        )
        .unwrap();
        for case in cases.as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut epoch_summary: Value =
                serde_json::from_str(include_str!("../tests/fixtures/epoch_summary_v2.json"))
                    .unwrap();
            if let Some(overrides) = case.get("epoch_summary_overrides") {
                for (key, value) in overrides.as_object().unwrap() {
                    epoch_summary[key] = value.clone();
                }
            }
            let epoch_summary: EpochSummary = serde_json::from_value(epoch_summary).unwrap();
            let result = validate_chain(&epoch_summary, &snapshot_of(&case["snapshot"]));
            match case["verdict"].as_str().unwrap() {
                "ok" => assert!(result.is_ok(), "{name}"),
                _ => assert_eq!(result.unwrap_err().to_string(), case["message"], "{name}"),
            }
        }
    }
}
