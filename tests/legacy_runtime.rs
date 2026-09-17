//! Runtime 393 uses primitive IDs instead of Finney's newer newtypes. Replay
//! its real metadata and storage offline, including signing and finalization.
mod support;

use serde_json::json;
use sn46_validator::burn::{BittensorBurnWriter, BurnFraction, Burner};
use sn46_validator::chain::{BittensorChain, Chain};
use support::{Node, fake_node::Cassette};

#[test]
fn runtime_393_reads_roster_and_finalizes_owner_burn() {
    let cassette = Cassette::load("localnet_5_spec393_1161877.json");
    let block = cassette.block_number;
    cassette.set(
        "chain_getBlockHash",
        json!([block]),
        json!(cassette.block_hash),
    );
    let wallet = tempfile::tempdir().unwrap();
    let hotkeys = wallet.path().join("validator/hotkeys");
    std::fs::create_dir_all(&hotkeys).unwrap();
    let raw = include_bytes!("fixtures/development_hotkey.json");
    std::fs::write(hotkeys.join("default"), raw).unwrap();
    let file: serde_json::Value = serde_json::from_slice(raw).unwrap();
    cassette.set(
        "system_accountNextIndex",
        json!([file["ss58Address"]]),
        json!(7),
    );
    let node = Node::new(cassette);
    node.set_events(true);
    let chain = BittensorChain::connect("local", &node.node.url).unwrap();
    let snapshot = chain.snapshot(5).unwrap();
    assert_eq!(snapshot.finalized_block, block);
    assert_eq!(snapshot.tempo, 10);
    assert_eq!(snapshot.last_step, 1161868);
    assert_eq!(snapshot.hotkeys.len(), 5);
    assert_eq!(
        snapshot.hotkeys[2],
        "5FnphvkdjkhHxd2kTRxgA3AAwRvzViZ4VzPfiiDYD231jfgj"
    );

    let state = wallet.path().join("state.json");
    let writer =
        BittensorBurnWriter::new(&chain, "validator", "default", wallet.path(), &state).unwrap();
    assert_eq!(
        writer.submit(5, block, &[], BurnFraction::FULL).unwrap(),
        "finalized"
    );
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(state.with_added_extension("submission.json")).unwrap(),
        "null"
    );
    node.runtime.block_on(async {
        let at = node.client.at_block(block).await.unwrap();
        let extrinsics = at.extrinsics().fetch().await.unwrap();
        let extrinsic = extrinsics.iter().next().unwrap().unwrap();
        assert_eq!(extrinsic.pallet_name(), "SubtensorModule");
        assert_eq!(extrinsic.call_name(), "set_mechanism_weights");
        let field = |name: &str| {
            extrinsic
                .iter_call_data_fields()
                .find(|field| field.name() == name)
                .unwrap()
        };
        assert_eq!(field("netuid").decode_as::<u16>().unwrap(), 5);
        assert_eq!(field("mecid").decode_as::<u8>().unwrap(), 0);
        assert_eq!(field("dests").decode_as::<Vec<u16>>().unwrap(), [0]);
        assert_eq!(field("weights").decode_as::<Vec<u16>>().unwrap(), [65535]);
        assert_eq!(field("version_key").decode_as::<u64>().unwrap(), 0);
        assert_eq!(
            extrinsic.transaction_extensions().unwrap().nonce().unwrap(),
            7
        );
    });
}
