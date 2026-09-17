//! Replays the finney cassette through the adapter and checks the decoded snapshot and the
//! contract's error strings.

mod support;

use serde_json::{Value as Json, json};
use sn46_validator::chain::{BittensorChain, Chain, ChainSnapshot};
use subxt::dynamic::{self, Value};
use subxt::ext::codec::Encode;
use subxt::rpcs::RpcClient;
use support::fake_node::{Cassette, FakeNode};

const CASSETTE: &str = "finney_46_9036625.json";
const NETUID: u64 = 46;

fn recorded_snapshot() -> Json {
    serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/finney_46_9036625_expected.json"
        ))
        .unwrap(),
    )
    .unwrap()
}

fn node() -> (Cassette, FakeNode) {
    let cassette = Cassette::load(CASSETTE);
    let node = FakeNode::spawn(cassette.clone());
    (cassette, node)
}

/// The storage key the client will ask for, from the cassette's own metadata.
fn storage_key(url: &str, name: &str, keys: Vec<Value>) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let rpc = RpcClient::new(subxt::rpcs::client::jsonrpsee_client(url).await.unwrap());
        let client = support::legacy_client(rpc).await;
        let at = client.at_current_block().await.unwrap();
        let address = dynamic::storage::<Vec<Value>, Value>("SubtensorModule", name);
        let key = at
            .storage()
            .entry(address)
            .unwrap()
            .fetch_key(keys)
            .unwrap();
        format!("0x{}", hex::encode(key))
    })
}

fn hex_scale<T: Encode>(value: T) -> Json {
    json!(format!("0x{}", hex::encode(value.encode())))
}

#[test]
fn bittensor_adapter_reads_finalized_storage() {
    let (cassette, node) = node();
    let expected = recorded_snapshot();
    let snapshot = BittensorChain::connect("local", &node.url)
        .unwrap()
        .snapshot(NETUID)
        .unwrap();
    assert_eq!(snapshot.finalized_block, cassette.block_number);
    let hotkeys: Vec<String> = expected["Keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_owned())
        .collect();
    let last_updates: Vec<u64> = expected["LastUpdate"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap())
        .collect();
    assert_eq!(
        snapshot,
        ChainSnapshot {
            finalized_block: expected["block_number"].as_u64().unwrap(),
            tempo: expected["Tempo"].as_u64().unwrap(),
            last_step: expected["LastMechansimStepBlock"].as_u64().unwrap(),
            hotkeys,
            last_updates,
        }
    );
    assert_eq!(
        snapshot.hotkeys.len(),
        expected["SubnetworkN"].as_u64().unwrap() as usize
    );
    assert_eq!(
        snapshot.finalized_block,
        snapshot.last_step + expected["BlocksSinceLastStep"].as_u64().unwrap()
    );
}

#[test]
fn finney_genesis_is_checked_by_network_name() {
    let (_cassette, node) = node();
    let failure = |network: &str, endpoint: &str| {
        BittensorChain::connect(network, endpoint)
            .err()
            .expect("connect fails")
            .to_string()
    };
    assert!(BittensorChain::connect("finney", &node.url).is_ok());
    assert_eq!(
        failure("test", &node.url),
        "chain genesis hash does not match network test"
    );
    assert_eq!(failure("nope", ""), "unknown network: nope");
    assert_eq!(failure("nope", &node.url), "unknown network: nope");
    assert!(failure("local", "ws://127.0.0.1:1").starts_with("chain connection failed: "));
}

#[test]
fn mutated_cassettes_fail_with_contract_messages() {
    let (cassette, node) = node();
    let netuid = Value::u128(u128::from(NETUID));
    let since = cassette.block_number
        - recorded_snapshot()["LastMechansimStepBlock"]
            .as_u64()
            .unwrap();
    let cases: [(&str, Vec<Value>, Json, &str); 7] = [
        (
            "BlocksSinceLastStep",
            vec![netuid.clone()],
            hex_scale(since + 1),
            "finalized epoch state is inconsistent",
        ),
        (
            "BlocksSinceLastStep",
            vec![netuid.clone()],
            hex_scale(u64::MAX),
            "finalized epoch state is inconsistent",
        ),
        (
            "Tempo",
            vec![netuid.clone()],
            hex_scale(0u16),
            "finalized epoch state is inconsistent",
        ),
        (
            "SubnetworkN",
            vec![netuid.clone()],
            hex_scale(0u16),
            "finalized subnet is empty or unknown",
        ),
        (
            "SubnetworkN",
            vec![netuid.clone()],
            hex_scale(255u16),
            "finalized hotkey roster is invalid",
        ),
        (
            "Keys",
            vec![netuid.clone(), Value::u128(5)],
            Json::Null,
            "finalized chain lookup failed: KeyError: 5",
        ),
        (
            "Keys",
            vec![netuid.clone(), Value::u128(200)],
            Json::Null,
            "finalized chain lookup failed: KeyError: 200",
        ),
    ];
    for (name, keys, result, message) in cases {
        let key = storage_key(&node.url, name, keys);
        let original = cassette.storage(&key).expect("recorded entry");
        cassette.set_storage(&key, result);
        let result = BittensorChain::connect("local", &node.url)
            .unwrap()
            .snapshot(NETUID);
        let error = result
            .err()
            .unwrap_or_else(|| panic!("{name} -> {message} did not fail"));
        assert_eq!(error.to_string(), message, "{name}");
        cassette.set_storage(&key, original);
    }
    assert!(
        BittensorChain::connect("local", &node.url)
            .unwrap()
            .snapshot(NETUID)
            .is_ok()
    );
}

/// The roster is a map by UID: the order a node answers a batch in does not matter, and
/// with several UIDs missing the lowest one is returned.
#[test]
fn roster_is_keyed_by_uid() {
    let (cassette, node) = node();
    cassette.reverse_batches();
    let snapshot = BittensorChain::connect("local", &node.url)
        .unwrap()
        .snapshot(NETUID)
        .unwrap();
    assert_eq!(json!(snapshot.hotkeys), recorded_snapshot()["Keys"]);
    for uid in [200, 5] {
        let key = storage_key(
            &node.url,
            "Keys",
            vec![Value::u128(u128::from(NETUID)), Value::u128(uid)],
        );
        cassette.set_storage(&key, Json::Null);
    }
    let error = BittensorChain::connect("local", &node.url)
        .unwrap()
        .snapshot(NETUID)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "finalized chain lookup failed: KeyError: 5"
    );
}

/// A request the cassette does not hold is an RPC error, never a silent default.
#[test]
fn unrecorded_requests_fail_loudly() {
    let (_cassette, node) = node();
    let error = BittensorChain::connect("local", &node.url)
        .unwrap()
        .snapshot(47)
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("finalized chain lookup failed: ")
            && error.contains("no entry for state_getStorage"),
        "{error}"
    );
}
