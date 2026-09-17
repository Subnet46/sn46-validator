//! The burn writer against the fake node: pre-check refusals, the decoded
//! extrinsic (call, dests, weights, version key, signer, mortal era), and both event
//! outcomes. Finalization alone never counts as success.

mod support;

use serde_json::json;
use sn46_shared::epoch_summary::EpochSummary;
use sn46_shared::scoring::{MinerScore, score_epoch_summary};
use sn46_validator::burn::{BittensorBurnWriter, BurnFraction, Burner, weights};
use sn46_validator::chain::BittensorChain;
use std::sync::atomic::Ordering;
use std::time::Duration;
use subxt::SubstrateConfig;
use subxt::config::transaction_extensions::CheckMortality;
use subxt::dynamic::Value;
use subxt::utils::Era;
use support::Node;
use support::fake_node::Cassette;

const CASSETTE: &str = "finney_46_9036625.json";
const NETUID: u64 = 46;
const BLOCK: u64 = 9036625;
const DEVELOPMENT_HOTKEY: &str = "5FnoWRL4FYzd29q8zXoY3JWio9PtBPgrAFu2RcgQazRQsiCs";
const OWNER_UID: u16 = 238;
const VERSION_KEY: u64 = 62;

/// A wallet directory holding the fixture hotkey as `validator/hotkeys/default`.
fn wallet() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let hotkeys = root.path().join("validator/hotkeys");
    std::fs::create_dir_all(&hotkeys).unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/development_hotkey.json"
        ),
        hotkeys.join("default"),
    )
    .unwrap();
    root
}

fn node() -> Node {
    let node = Node::new(Cassette::load(CASSETTE));
    node.cassette.set(
        "chain_getBlockHash",
        json!([BLOCK]),
        json!(node.cassette.block_hash),
    );
    node.cassette.set(
        "system_accountNextIndex",
        json!([DEVELOPMENT_HOTKEY]),
        json!(7),
    );
    // The recorded subnet has commit-reveal enabled; exercise writes with it off.
    node.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    node
}

fn chain(node: &Node) -> BittensorChain {
    BittensorChain::connect("local", &node.node.url).unwrap()
}

fn scored() -> Vec<MinerScore> {
    let epoch_summary: EpochSummary =
        serde_json::from_str(include_str!("fixtures/epoch_summary_v2.json")).unwrap();
    score_epoch_summary(&epoch_summary)
}

/// `(dests, weights)` of the `set_mechanism_weights` call in the fake block.
fn submitted_weights(node: &Node) -> (Vec<u16>, Vec<u16>) {
    node.runtime.block_on(async {
        let at = node.client.at_block(BLOCK).await.unwrap();
        let extrinsics = at.extrinsics().fetch().await.unwrap();
        let extrinsic = extrinsics.iter().next().unwrap().unwrap();
        let field = |name: &str| {
            extrinsic
                .iter_call_data_fields()
                .find(|field| field.name() == name)
                .unwrap_or_else(|| panic!("field {name}"))
                .decode_as::<Vec<u16>>()
                .unwrap()
        };
        (field("dests"), field("weights"))
    })
}

#[test]
fn writer_submits_the_full_owner_burn_vector() {
    let node = node();
    node.set_events(true);
    let wallet = wallet();
    let chain = chain(&node);
    let writer = BittensorBurnWriter::new(
        &chain,
        "validator",
        "default",
        wallet.path(),
        wallet.path().join("state.json"),
    )
    .unwrap();
    assert_eq!(writer.hotkey(), DEVELOPMENT_HOTKEY);

    let result = writer
        .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
        .unwrap();

    assert_eq!(result, "finalized");
    let submissions = node.node.submissions.lock().unwrap().clone();
    assert_eq!(submissions.len(), 1);
    node.runtime.block_on(async {
        let at = node.client.at_block(BLOCK).await.unwrap();
        let extrinsics = at.extrinsics().fetch().await.unwrap();
        let extrinsic = extrinsics.iter().next().unwrap().unwrap();
        assert_eq!(extrinsic.pallet_name(), "SubtensorModule");
        assert_eq!(extrinsic.call_name(), "set_mechanism_weights");
        let field = |name: &str| {
            extrinsic
                .iter_call_data_fields()
                .find(|field| field.name() == name)
                .unwrap_or_else(|| panic!("field {name}"))
        };
        assert_eq!(field("netuid").decode_as::<u16>().unwrap(), 46);
        assert_eq!(field("mecid").decode_as::<u8>().unwrap(), 0);
        assert_eq!(field("dests").decode_as::<Vec<u16>>().unwrap(), [OWNER_UID]);
        assert_eq!(field("weights").decode_as::<Vec<u16>>().unwrap(), [65_535]);
        assert_eq!(
            field("version_key").decode_as::<u64>().unwrap(),
            VERSION_KEY
        );
        let signer = extrinsic.address_bytes().unwrap();
        let (development, _) =
            sn46_shared::identity::decode_registered_ss58(DEVELOPMENT_HOTKEY).unwrap();
        assert_eq!(
            signer,
            [&[0u8][..], &development[..]].concat(),
            "MultiAddress::Id(hotkey)"
        );
        let era = extrinsic
            .transaction_extensions()
            .unwrap()
            .find::<CheckMortality<SubstrateConfig>>()
            .unwrap()
            .unwrap();
        assert_eq!(era, Era::mortal(64, BLOCK));
        let nonce = extrinsic.transaction_extensions().unwrap().nonce().unwrap();
        assert_eq!(nonce, 7, "nonce from system_accountNextIndex");
    });
}

#[test]
fn writer_submits_the_scored_vector_with_the_burn_share() {
    for bps in [0, 2_500, 5_000, 10_000] {
        let node = node();
        node.set_events(true);
        let wallet = wallet();
        let chain = chain(&node);
        let writer = BittensorBurnWriter::new(
            &chain,
            "validator",
            "default",
            wallet.path(),
            wallet.path().join("state.json"),
        )
        .unwrap();
        let records = scored();
        let burn = BurnFraction::from_bps(bps).unwrap();

        let result = writer.submit(NETUID, BLOCK, &records, burn).unwrap();

        assert_eq!(result, "finalized");
        let expected = weights(&records, OWNER_UID, burn).unwrap();
        assert_eq!(expected.0.contains(&OWNER_UID), bps > 0);
        assert_eq!(
            expected.0.len(),
            if bps == 10_000 {
                1
            } else {
                2 + usize::from(bps > 0)
            }
        );
        assert_eq!(submitted_weights(&node), expected, "burn {bps} bps");
    }
}

#[test]
fn writer_refuses_non_burn_subnets() {
    let node = node();
    let wallet = wallet();
    let chain = chain(&node);
    let writer = BittensorBurnWriter::new(
        &chain,
        "validator",
        "default",
        wallet.path(),
        wallet.path().join("state.json"),
    )
    .unwrap();
    node.set_subtensor("RecycleOrBurn", vec![], 1u8);
    assert_eq!(
        writer
            .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
            .unwrap_err()
            .0,
        "Subnet is not in Burn mode"
    );
    assert!(node.node.submissions.lock().unwrap().is_empty());
}

fn enable_commit_reveal(node: &Node) {
    node.set_subtensor("CommitRevealWeightsEnabled", vec![], true);
    node.set_subtensor("LastEpochBlock", vec![], BLOCK - 100);
    node.set_subtensor("PendingEpochAt", vec![], 0u64);
    node.set_subtensor("SubnetEpochIndex", vec![], 100u64);
    node.set_subtensor("RevealPeriodEpochs", vec![], 1u64);
    node.set_subtensor("BlocksSinceLastStep", vec![], 100u64);
    let key = node.storage_key("SubtensorModule", "CommitRevealWeightsVersion", vec![]);
    node.cassette.set_storage(&key, json!("0x0400"));
    node.cassette.set(
        "chain_getBlockHash",
        json!([null]),
        json!(node.cassette.block_hash),
    );
}

#[test]
fn writer_submits_timelocked_weights_when_commit_reveal_is_enabled() {
    let node = node();
    enable_commit_reveal(&node);
    node.set_events(true);
    let wallet = wallet();
    let chain = chain(&node);
    let writer = BittensorBurnWriter::new(
        &chain,
        "validator",
        "default",
        wallet.path(),
        wallet.path().join("state.json"),
    )
    .unwrap();
    assert_eq!(
        writer
            .submit(NETUID, BLOCK, &scored(), BurnFraction::FULL)
            .unwrap(),
        "commit finalized; chain will reveal weights"
    );
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    node.runtime.block_on(async {
        let at = node.client.at_block(BLOCK).await.unwrap();
        let extrinsics = at.extrinsics().fetch().await.unwrap();
        let extrinsic = extrinsics.iter().next().unwrap().unwrap();
        assert_eq!(extrinsic.call_name(), "commit_timelocked_mechanism_weights");
        let field = |name: &str| {
            extrinsic
                .iter_call_data_fields()
                .find(|field| field.name() == name)
                .unwrap()
        };
        assert_eq!(field("netuid").decode_as::<u16>().unwrap(), 46);
        assert_eq!(field("mecid").decode_as::<u8>().unwrap(), 0);
        assert_eq!(
            field("commit_reveal_version").decode_as::<u16>().unwrap(),
            4
        );
        assert!(!field("commit").decode_as::<Vec<u8>>().unwrap().is_empty());
        let round = field("reveal_round").decode_as::<u64>().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(round * 3 + 1_692_803_367 > now);
    });
}

#[test]
fn unknown_commit_reveal_protocol_is_refused_before_submission() {
    let node = node();
    enable_commit_reveal(&node);
    let key = node.storage_key("SubtensorModule", "CommitRevealWeightsVersion", vec![]);
    node.cassette.set_storage(&key, json!("0x0500"));
    let wallet = wallet();
    let chain = chain(&node);
    let writer = BittensorBurnWriter::new(
        &chain,
        "validator",
        "default",
        wallet.path(),
        wallet.path().join("state.json"),
    )
    .unwrap();
    assert!(
        writer
            .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
            .unwrap_err()
            .0
            .contains("unsupported commit/reveal version 5")
    );
    assert!(node.node.submissions.lock().unwrap().is_empty());
}

#[test]
fn stalled_submission_times_out_and_a_restart_cannot_submit_a_duplicate() {
    let node = node();
    node.node.stall_submissions.store(true, Ordering::Relaxed);
    let wallet = wallet();
    let state_path = wallet.path().join("state.json");
    {
        let chain = chain(&node);
        let writer =
            BittensorBurnWriter::new(&chain, "validator", "default", wallet.path(), &state_path)
                .unwrap()
                .with_timeout(Duration::from_secs(1));
        assert!(
            writer
                .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
                .unwrap_err()
                .0
                .contains("timed out")
        );
    }
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    let chain = chain(&node);
    let writer =
        BittensorBurnWriter::new(&chain, "validator", "default", wallet.path(), &state_path)
            .unwrap();
    assert!(
        writer
            .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
            .unwrap_err()
            .0
            .contains("Previous submission is uncertain")
    );
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    let guard: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state_path.with_added_extension("submission.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(guard["expires_at"], BLOCK + 64);
    assert_eq!(guard["hotkey"], DEVELOPMENT_HOTKEY);
    // The exact mortal era has now expired at the finalized head. A retry is safe.
    let expired = BLOCK + 64;
    node.cassette.set(
        "chain_getBlockHash",
        json!([expired]),
        json!(node.cassette.block_hash),
    );
    let mut header = node
        .cassette
        .get("chain_getHeader", json!([node.cassette.block_hash]))
        .unwrap();
    header["number"] = json!(format!("0x{expired:x}"));
    node.cassette
        .set("chain_getHeader", json!([node.cassette.block_hash]), header);
    node.node.stall_submissions.store(false, Ordering::Relaxed);
    node.set_events(true);
    assert_eq!(
        writer
            .submit(NETUID, expired, &[], BurnFraction::FULL)
            .unwrap(),
        "finalized"
    );
    assert_eq!(node.node.submissions.lock().unwrap().len(), 2);
    assert_eq!(
        std::fs::read_to_string(state_path.with_added_extension("submission.json")).unwrap(),
        "null"
    );
}

#[test]
fn missing_owner_or_uid_is_refused_before_signing() {
    let node = node();
    let wallet = wallet();
    let chain = chain(&node);
    let writer = BittensorBurnWriter::new(
        &chain,
        "validator",
        "default",
        wallet.path(),
        wallet.path().join("state.json"),
    )
    .unwrap();
    let owner_key = node.storage_key(
        "SubtensorModule",
        "SubnetOwnerHotkey",
        vec![Value::u128(u128::from(NETUID))],
    );
    let owner = node.cassette.storage(&owner_key).unwrap();
    node.cassette.set_storage(&owner_key, json!(null));
    assert_eq!(
        writer
            .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
            .unwrap_err()
            .0,
        "Subnet owner hotkey is invalid"
    );
    node.cassette.set_storage(&owner_key, owner.clone());
    let owner_bytes = hex::decode(owner.as_str().unwrap().trim_start_matches("0x")).unwrap();
    let uid_key = node.storage_key(
        "SubtensorModule",
        "Uids",
        vec![
            Value::u128(u128::from(NETUID)),
            Value::from_bytes(owner_bytes),
        ],
    );
    let uid = node.cassette.storage(&uid_key).unwrap();
    node.cassette.set_storage(&uid_key, json!(null));
    assert_eq!(
        writer
            .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
            .unwrap_err()
            .0,
        "Subnet owner UID or weights version is invalid"
    );
    node.cassette.set_storage(&uid_key, uid);
    node.set_subtensor("WeightsVersionKey", vec![], 63u64);
    assert!(node.node.submissions.lock().unwrap().is_empty());
}

#[test]
fn extrinsic_failed_is_a_burn_error_and_finalization_alone_is_not_success() {
    for case in 0..4 {
        let node = node();
        let wallet = wallet();
        let chain = chain(&node);
        let writer = BittensorBurnWriter::new(
            &chain,
            "validator",
            "default",
            wallet.path(),
            wallet.path().join("state.json"),
        )
        .unwrap();
        let events_key = node.storage_key("System", "Events", vec![]);
        match case {
            0 => {} // Missing RPC response: outcome cannot be read.
            1 => node.cassette.set_storage(&events_key, json!(null)),
            2 => node.cassette.set_storage(&events_key, json!("0x00")),
            _ => node.set_events(false),
        }
        let error = writer
            .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
            .unwrap_err()
            .0;
        assert!(error.starts_with("Burn write failed: "), "{error}");
        if case == 1 || case == 2 {
            assert!(error.contains("ExtrinsicSuccess"), "{error}");
        }
        if case == 3 {
            assert!(
                error.contains("BadOrigin") || error.contains("Bad origin"),
                "{error}"
            );
        }
        assert!(
            writer
                .submit(NETUID, BLOCK, &[], BurnFraction::FULL)
                .unwrap_err()
                .0
                .contains("Previous submission is uncertain")
        );
        assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    }
}

#[test]
fn hotkey_files_are_checked_before_any_chain_contact() {
    let node = node();
    let chain = chain(&node);
    let wallet = wallet();
    let file = wallet.path().join("validator/hotkeys/default");
    let good = std::fs::read(&file).unwrap();
    let failure = |contents: &[u8]| {
        std::fs::write(&file, contents).unwrap();
        BittensorBurnWriter::new(
            &chain,
            "validator",
            "default",
            wallet.path(),
            wallet.path().join("state.json"),
        )
        .err()
        .expect("refused")
        .0
    };
    assert!(failure(b"$NACL...").contains("encrypted"));
    assert_eq!(
        failure(b"{}"),
        "Validator hotkey file has no 32-byte secretSeed"
    );
    let swapped = String::from_utf8(good.clone()).unwrap().replace(
        DEVELOPMENT_HOTKEY,
        "5DAAnrj7VHTznn2AWBemMuyBwZWs6FNFjdyVXUeYum3PTXFy",
    );
    assert_eq!(
        failure(swapped.as_bytes()),
        "Validator hotkey file ss58Address does not match its secretSeed"
    );
    assert!(
        BittensorBurnWriter::new(
            &chain,
            "validator",
            "missing",
            wallet.path(),
            wallet.path().join("state.json")
        )
        .err()
        .unwrap()
        .0
        .contains("is unreadable")
    );
}
