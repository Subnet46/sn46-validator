//! Records one finalized block through the Rust client into an offline node cassette.
//! Both recorders perform reads only; neither signs nor submits a transaction.

mod support;

use serde_json::json;
use subtensor::api::runtime_types::pallet_subtensor::pallet::RecycleOrBurnEnum;
use subxt::dynamic;
use subxt::rpcs::RpcClient;
use subxt::utils::AccountId32;

#[tokio::test]
#[ignore]
async fn record_finney_46() {
    record("wss://entrypoint-finney.opentensor.ai:443", 46, "finney_46").await;
}

/// Read-only recorder; never signs or submits a transaction.
#[tokio::test]
#[ignore]
async fn record_localnet_393() {
    let endpoint = std::env::var("LOCALNET_ENDPOINT").expect("LOCALNET_ENDPOINT is required");
    record(&endpoint, 5, "localnet_5_spec393").await;
}

async fn record(endpoint: &str, netuid: u16, label: &str) {
    let recording = support::Recording::connect(endpoint).await;
    let entries = recording.entries.clone();
    let rpc = RpcClient::new(recording);
    let client = support::legacy_client(rpc.clone()).await;
    let at = client.at_current_block().await.unwrap();
    let (block, hash) = (at.block_number(), at.block_hash());
    let storage = at.storage();

    let key = || (netuid,);
    let n: u16 = storage
        .fetch(
            dynamic::storage::<(u16,), u16>("SubtensorModule", "SubnetworkN"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let tempo: u16 = storage
        .fetch(
            dynamic::storage::<(u16,), u16>("SubtensorModule", "Tempo"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let last_step: u64 = storage
        .fetch(
            dynamic::storage::<(u16,), u64>("SubtensorModule", "LastMechansimStepBlock"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let since: u64 = storage
        .fetch(
            dynamic::storage::<(u16,), u64>("SubtensorModule", "BlocksSinceLastStep"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let last_update: Vec<u64> = storage
        .fetch(
            dynamic::storage::<(u16,), Vec<u64>>("SubtensorModule", "LastUpdate"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    // The roster is paged in exactly as `BittensorChain` reads it.
    let mut roster = 0u16;
    let mut keys = storage
        .iter(
            dynamic::storage::<(u16, u16), AccountId32>("SubtensorModule", "Keys"),
            key(),
        )
        .await
        .unwrap();
    while let Some(entry) = keys.next().await {
        entry.unwrap();
        roster += 1;
    }
    let mode = storage
        .fetch(
            dynamic::storage::<(u16,), RecycleOrBurnEnum>("SubtensorModule", "RecycleOrBurn"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let commit_reveal: bool = storage
        .fetch(
            dynamic::storage::<(u16,), bool>("SubtensorModule", "CommitRevealWeightsEnabled"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let owner: AccountId32 = storage
        .fetch(
            dynamic::storage::<(u16,), AccountId32>("SubtensorModule", "SubnetOwnerHotkey"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let owner_uid: u16 = storage
        .fetch(
            dynamic::storage::<(u16, AccountId32), u16>("SubtensorModule", "Uids"),
            (netuid, owner),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let version_key: u64 = storage
        .fetch(
            dynamic::storage::<(u16,), u64>("SubtensorModule", "WeightsVersionKey"),
            key(),
        )
        .await
        .unwrap()
        .decode()
        .unwrap();
    let nonce = at.tx().account_nonce(&owner).await.unwrap();
    let next_index = support::legacy_methods(rpc)
        .system_account_next_index(&owner)
        .await
        .unwrap();

    let cassette = json!({
        "block_number": block,
        "block_hash": format!("{hash:?}"),
        "entries": *entries.lock().unwrap(),
    });
    let path = format!(
        "{}/tests/golden/{label}_{block}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::write(&path, serde_json::to_vec(&cassette).unwrap()).unwrap();

    let mut methods: Vec<String> = entries
        .lock()
        .unwrap()
        .iter()
        .map(|entry| entry["method"].as_str().unwrap().to_owned())
        .collect();
    methods.sort();
    methods.dedup();
    println!(
        "block={block} hash={hash:?} n={n} tempo={tempo} last_step={last_step} since={since} last_update_len={} roster={roster}",
        last_update.len()
    );
    println!(
        "mode={mode:?} commit_reveal={commit_reveal} owner={owner} owner_uid={owner_uid} version_key={version_key} owner_nonce={nonce} owner_next_index={next_index}"
    );
    println!(
        "methods={methods:?} entries={} path={path}",
        entries.lock().unwrap().len()
    );
}
