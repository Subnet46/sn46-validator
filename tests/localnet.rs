#[path = "support/localnet.rs"]
mod localnet;
mod support;

use std::time::Duration;

use serde_json::Value as Json;
use sn46_shared::identity::{SS58_FORMAT, encode_ss58};
use subxt::config::DefaultExtrinsicParamsBuilder;
use subxt::dynamic::{self, Value};
use subxt::ext::scale_value::{Composite, ValueDef};
use subxt::extrinsics::ExtrinsicEvents;
use subxt::rpcs::RpcClient;
use subxt::transactions::Payload;
use subxt::{OnlineClient, SubstrateConfig};
use subxt_signer::sr25519::Keypair;

/// Public test-only seed [0x42; 32]; never use this wallet outside isolated tests.
/// `from_secret_key` on a hotkey file yields its `ss58Address`.
#[test]
fn hotkey_file_derives_its_ss58_address() {
    let raw = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/development_hotkey.json"
    ))
    .unwrap();
    let file: Json = serde_json::from_slice(&raw).unwrap();
    let seed = hex::decode(
        file["secretSeed"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
    )
    .unwrap();
    let keypair = Keypair::from_secret_key(seed.try_into().unwrap()).unwrap();
    assert_eq!(
        encode_ss58(&keypair.public_key().0, SS58_FORMAT),
        file["ss58Address"]
    );
    assert_eq!(
        file["ss58Address"],
        "5FnoWRL4FYzd29q8zXoY3JWio9PtBPgrAFu2RcgQazRQsiCs"
    );
}

/// Signs `call` as `signer` with a pool-aware nonce and an immortal era, waits for
/// finalization, and returns the extrinsic's events.
async fn finalize(
    client: &OnlineClient<SubstrateConfig>,
    rpc: &RpcClient,
    signer: &Keypair,
    call: &impl Payload,
) -> ExtrinsicEvents<SubstrateConfig> {
    // The nonce must come from the pool-aware `system_accountNextIndex`:
    // `AccountNonceApi_account_nonce` at the finalized block is stale while earlier
    // transactions from the same account are still being finalized ("Transaction is outdated").
    let nonce = support::legacy_methods(rpc.clone())
        .system_account_next_index(&signer.public_key().to_account_id())
        .await
        .unwrap();
    let params = DefaultExtrinsicParamsBuilder::<SubstrateConfig>::new()
        .immortal()
        .nonce(nonce)
        .build();
    let at = client.at_current_block().await.unwrap();
    let mut tx = at.tx();
    let signed = tx.create_signed(call, signer, params).await.unwrap();
    let finalized = signed
        .submit_and_watch()
        .await
        .unwrap()
        .wait_for_finalized()
        .await
        .unwrap();
    finalized.wait_for_success().await.unwrap()
}

/// The decoded fields of the first `pallet::name` event among `events`.
fn event_fields(
    events: &ExtrinsicEvents<SubstrateConfig>,
    pallet: &str,
    name: &str,
) -> Composite<()> {
    events
        .iter()
        .map(Result::unwrap)
        .find(|event| event.pallet_name() == pallet && event.event_name() == name)
        .unwrap_or_else(|| panic!("no {pallet}::{name} event"))
        .decode_fields_unchecked_as()
        .unwrap()
}

/// `Sudo::sudo(AdminUtils::<name>(args))` as Alice, retried while the chain refuses admin
/// calls in the protected weights window at the end of each tempo.
async fn sudo(
    client: &OnlineClient<SubstrateConfig>,
    rpc: &RpcClient,
    alice: &Keypair,
    name: &str,
    args: Vec<(&str, Value)>,
) {
    for _ in 0..60 {
        let inner =
            Value::unnamed_variant("AdminUtils", vec![Value::named_variant(name, args.clone())]);
        let call = dynamic::tx("Sudo", "sudo", vec![inner]);
        let events = finalize(client, rpc, alice, &call).await;
        let Composite::Named(fields) = event_fields(&events, "Sudo", "Sudid") else {
            panic!("Sudid has named fields");
        };
        let (_, result) = fields
            .iter()
            .find(|(field, _)| field == "sudo_result")
            .unwrap();
        match &result.value {
            ValueDef::Variant(verdict) if verdict.name == "Ok" => return,
            ValueDef::Variant(verdict) => println!("{name}: {:?}, retrying", verdict.values),
            other => panic!("sudo_result is not a Result: {other:?}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("{name} kept failing");
}

/// Registers a subnet as `//Alice`, turns commit-reveal off and the weights rate limit to
/// zero through sudo, then submits `set_mechanism_weights` as Alice's hotkey and checks it
/// finalized. Starts an isolated Subtensor localnet; requires Docker.
#[tokio::test]
async fn localnet_burn_extrinsic_finalizes() {
    let (_node, rpc) = localnet::Localnet::start().await;
    tokio::time::timeout(Duration::from_secs(180), burn_extrinsic_finalizes(rpc))
        .await
        .expect("localnet weights test did not finish within 180 seconds");
}

/// Exercise the real writer with commit/reveal enabled, including its mortal era,
/// typed storage reads, timelock payload, and durable submission guard.
#[tokio::test]
async fn localnet_timelocked_burn_writer_finalizes() {
    let (node, rpc) = localnet::Localnet::start().await;
    tokio::time::timeout(Duration::from_secs(180), async {
        use sn46_validator::burn::{BittensorBurnWriter, BurnFraction, Burner};
        use sn46_validator::chain::BittensorChain;
        let client = support::legacy_client(rpc.clone()).await;
        let alice = subxt_signer::sr25519::dev::alice();
        let wallet = tempfile::tempdir().unwrap();
        let hotkeys = wallet.path().join("validator/hotkeys");
        std::fs::create_dir_all(&hotkeys).unwrap();
        let raw = include_bytes!("fixtures/development_hotkey.json");
        std::fs::write(hotkeys.join("default"), raw).unwrap();
        let fixture: Json = serde_json::from_slice(raw).unwrap();
        let address = fixture["ss58Address"].as_str().unwrap();
        let (account, _) = sn46_shared::identity::decode_registered_ss58(address).unwrap();
        let hotkey = Value::from_bytes(account);
        finalize(
            &client,
            &rpc,
            &alice,
            &dynamic::tx(
                "Balances",
                "transfer_allow_death",
                vec![
                    Value::unnamed_variant("Id", vec![hotkey.clone()]),
                    Value::u128(10_000_000_000),
                ],
            ),
        )
        .await;
        let registered = finalize(
            &client,
            &rpc,
            &alice,
            &dynamic::tx("SubtensorModule", "register_network", vec![hotkey]),
        )
        .await;
        let added = registered
            .find_first::<subtensor::api::subtensor_module::events::NetworkAdded>()
            .unwrap()
            .unwrap();
        let netuid = u128::from(added.0.0);
        let at = client.at_current_block().await.unwrap();
        let enabled: bool = at
            .storage()
            .fetch(
                dynamic::storage::<Vec<Value>, Value>(
                    "SubtensorModule",
                    "CommitRevealWeightsEnabled",
                ),
                vec![Value::u128(netuid)],
            )
            .await
            .unwrap()
            .decode_as()
            .unwrap();
        assert!(enabled, "new subnets must start with commit/reveal enabled");
        // The isolated fixture hotkey has no stake; remove the global stake gate
        // in this localnet just as Subtensor's commit/reveal tests do.
        sudo(
            &client,
            &rpc,
            &alice,
            "sudo_set_stake_threshold",
            vec![("min_stake", Value::u128(0))],
        )
        .await;
        sudo(
            &client,
            &rpc,
            &alice,
            "sudo_set_weights_set_rate_limit",
            vec![
                ("netuid", Value::u128(netuid)),
                ("weights_set_rate_limit", Value::u128(0)),
            ],
        )
        .await;
        let block = client.at_current_block().await.unwrap().block_number();
        let endpoint = node.url.clone();
        let result = tokio::task::spawn_blocking(move || {
            let chain = BittensorChain::connect("local", &endpoint).unwrap();
            let state_path = wallet.path().join("state.json");
            let writer = BittensorBurnWriter::new(
                &chain,
                "validator",
                "default",
                wallet.path(),
                &state_path,
            )
            .unwrap();
            let result = writer
                .submit(netuid as u64, block, &[], BurnFraction::FULL)
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(state_path.with_added_extension("submission.json"))
                    .unwrap(),
                "null"
            );
            result
        })
        .await
        .unwrap();
        assert_eq!(result, "commit finalized; chain will reveal weights");
        let at = client.at_current_block().await.unwrap();
        let updates: Vec<u64> = at
            .storage()
            .fetch(
                dynamic::storage::<Vec<Value>, Value>("SubtensorModule", "LastUpdate"),
                vec![Value::u128(netuid)],
            )
            .await
            .unwrap()
            .decode_as()
            .unwrap();
        assert!(updates.iter().any(|update| *update >= block));
    })
    .await
    .expect("timelocked burn writer did not finish within 180 seconds");
}

async fn burn_extrinsic_finalizes(rpc: RpcClient) {
    let client = support::legacy_client(rpc.clone()).await;
    let alice = subxt_signer::sr25519::dev::alice();
    let hotkey = Value::from_bytes(alice.public_key().0);

    let registered = finalize(
        &client,
        &rpc,
        &alice,
        &dynamic::tx("SubtensorModule", "register_network", vec![hotkey.clone()]),
    )
    .await;
    let added = registered
        .find_first::<subtensor::api::subtensor_module::events::NetworkAdded>()
        .unwrap()
        .unwrap();
    let netuid = u128::from(added.0.0);
    sudo(
        &client,
        &rpc,
        &alice,
        "sudo_set_commit_reveal_weights_enabled",
        vec![
            ("netuid", Value::u128(netuid)),
            ("enabled", Value::bool(false)),
        ],
    )
    .await;
    sudo(
        &client,
        &rpc,
        &alice,
        "sudo_set_weights_set_rate_limit",
        vec![
            ("netuid", Value::u128(netuid)),
            ("weights_set_rate_limit", Value::u128(0)),
        ],
    )
    .await;

    let at = client.at_current_block().await.unwrap();
    let storage = at.storage();
    let plain = |name: &str| dynamic::storage::<Vec<Value>, Value>("SubtensorModule", name);
    let owner_uid: u16 = storage
        .fetch(plain("Uids"), vec![Value::u128(netuid), hotkey])
        .await
        .unwrap()
        .decode_as()
        .unwrap();
    let version_key: u64 = storage
        .fetch(plain("WeightsVersionKey"), vec![Value::u128(netuid)])
        .await
        .unwrap()
        .decode_as()
        .unwrap();
    let commit_reveal: bool = storage
        .fetch(
            plain("CommitRevealWeightsEnabled"),
            vec![Value::u128(netuid)],
        )
        .await
        .unwrap()
        .decode_as()
        .unwrap();
    assert!(!commit_reveal, "commit-reveal is still enabled");
    println!("netuid={netuid} owner_uid={owner_uid} version_key={version_key}");

    let metadata = at.metadata();
    let extrinsic = metadata.extrinsic();
    let extensions: Vec<&str> = extrinsic
        .transaction_extensions_by_version(
            extrinsic.transaction_extension_version_to_use_for_encoding(),
        )
        .expect("extension version")
        .map(|extension| extension.identifier())
        .collect();
    println!("transaction extensions: {extensions:?}");

    let call = dynamic::tx(
        "SubtensorModule",
        "set_mechanism_weights",
        vec![
            Value::u128(netuid),
            Value::u128(0),
            Value::unnamed_composite(vec![Value::u128(owner_uid.into())]),
            Value::unnamed_composite(vec![Value::u128(65_535)]),
            Value::u128(version_key.into()),
        ],
    );
    let nonce = support::legacy_methods(rpc)
        .system_account_next_index(&alice.public_key().to_account_id())
        .await
        .unwrap();
    let params = DefaultExtrinsicParamsBuilder::<SubstrateConfig>::new()
        .immortal()
        .nonce(nonce)
        .build();
    let mut tx = at.tx();
    let signed = tx.create_signed(&call, &alice, params).await.unwrap();
    let encoded = signed.encoded().to_vec();
    // compact length, 0x84, MultiAddress::Id (1 + 32), MultiSignature::Sr25519 (1 + 64), then the era.
    let prefix = match encoded[0] & 0b11 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => panic!("big-int compact length"),
    };
    assert_eq!(encoded[prefix], 0x84);
    assert_eq!(encoded[prefix + 1], 0x00);
    assert_eq!(encoded[prefix + 34], 0x01);
    let era = encoded[prefix + 99];
    println!("extrinsic bytes={} era byte=0x{era:02x}", encoded.len());
    assert_eq!(era, 0x00, "era must be immortal");

    let finalized = signed
        .submit_and_watch()
        .await
        .unwrap()
        .wait_for_finalized()
        .await
        .unwrap();
    println!(
        "finalized in block {:?} extrinsic {:?}",
        finalized.block_hash(),
        finalized.extrinsic_hash()
    );
    finalized.wait_for_success().await.unwrap();

    let after = client.at_current_block().await.unwrap();
    let last_update: Vec<u64> = after
        .storage()
        .fetch(
            dynamic::storage::<Vec<Value>, Value>("SubtensorModule", "LastUpdate"),
            vec![Value::u128(netuid)],
        )
        .await
        .unwrap()
        .decode_as()
        .unwrap();
    println!("LastUpdate={last_update:?}");
    assert!(last_update[usize::from(owner_uid)] > 0);
}
