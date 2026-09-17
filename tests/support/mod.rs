//! Chain test helpers: a recording RPC transport and the legacy-only client bootstrap.

#![allow(dead_code)]

pub mod fake_node;

use std::sync::{Arc, Mutex};

use fake_node::{Cassette, FakeNode};
use serde_json::{Value, json};
use subxt::backend::LegacyBackend;
use subxt::config::RpcConfigFor;
use subxt::dynamic::{self, Value as ScaleValue};
use subxt::ext::codec::Encode;
use subxt::ext::scale_value::{Composite, scale::encode_as_type};
use subxt::rpcs::RpcClient;
use subxt::rpcs::client::{
    JsonrpseeRpcClient, RawRpcFuture, RawRpcSubscription, RawValue, RpcClientT,
};
use subxt::rpcs::methods::legacy::LegacyRpcMethods;
use subxt::{OnlineClient, SubstrateConfig};

/// Wraps a real transport and appends every request as `{method, params, result}`.
pub struct Recording {
    inner: JsonrpseeRpcClient,
    pub entries: Arc<Mutex<Vec<Value>>>,
}

impl Recording {
    pub async fn connect(url: &str) -> Self {
        let inner = subxt::rpcs::client::jsonrpsee_client(url)
            .await
            .expect("connect");
        Self {
            inner,
            entries: Arc::default(),
        }
    }
}

impl RpcClientT for Recording {
    fn request_raw<'a>(
        &'a self,
        method: &'a str,
        params: Option<Box<RawValue>>,
    ) -> RawRpcFuture<'a, Box<RawValue>> {
        Box::pin(async move {
            let sent: Value = match &params {
                Some(raw) => serde_json::from_str(raw.get()).expect("params are JSON"),
                None => Value::Null,
            };
            let result = self.inner.request_raw(method, params).await?;
            let received: Value = serde_json::from_str(result.get()).expect("result is JSON");
            self.entries
                .lock()
                .unwrap()
                .push(json!({"method": method, "params": sent, "result": received}));
            Ok(result)
        })
    }

    fn subscribe_raw<'a>(
        &'a self,
        sub: &'a str,
        params: Option<Box<RawValue>>,
        unsub: &'a str,
    ) -> RawRpcFuture<'a, RawRpcSubscription> {
        // Subscriptions are not recorded; reads never use them.
        self.inner.subscribe_raw(sub, params, unsub)
    }
}

/// The validator's client: legacy RPC only, so the traffic is deterministic and replayable.
pub async fn legacy_client(rpc: RpcClient) -> OnlineClient<SubstrateConfig> {
    let backend = LegacyBackend::<SubstrateConfig>::builder().build(rpc);
    OnlineClient::from_backend(Arc::new(backend))
        .await
        .expect("client bootstrap")
}

/// Raw legacy RPC methods on the same transport (for `system_accountNextIndex`).
pub fn legacy_methods(rpc: RpcClient) -> LegacyRpcMethods<RpcConfigFor<SubstrateConfig>> {
    LegacyRpcMethods::new(rpc)
}

/// A cassette-backed node with a client for mutating SN46 storage and extrinsic events.
pub struct Node {
    pub cassette: Cassette,
    pub node: FakeNode,
    pub runtime: tokio::runtime::Runtime,
    pub client: subxt::OnlineClient<SubstrateConfig>,
}

impl Node {
    pub fn new(cassette: Cassette) -> Self {
        let node = FakeNode::spawn(cassette.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = runtime.block_on(async {
            let rpc = RpcClient::new(
                subxt::rpcs::client::jsonrpsee_client(&node.url)
                    .await
                    .unwrap(),
            );
            legacy_client(rpc).await
        });
        Self {
            cassette,
            node,
            runtime,
            client,
        }
    }

    pub fn storage_key(&self, pallet: &str, name: &str, keys: Vec<ScaleValue>) -> String {
        self.runtime.block_on(async {
            let at = self.client.at_current_block().await.unwrap();
            let address = dynamic::storage::<Vec<ScaleValue>, ScaleValue>(pallet, name);
            let key = at
                .storage()
                .entry(address)
                .unwrap()
                .fetch_key(keys)
                .unwrap();
            format!("0x{}", hex::encode(key))
        })
    }

    /// `System.Events` at the cassette block: one record for extrinsic 0, success or
    /// `ExtrinsicFailed(BadOrigin)`, SCALE-encoded against the recorded metadata.
    pub fn set_events(&self, success: bool) {
        let dispatch_info = ScaleValue::named_composite([
            (
                "weight",
                ScaleValue::named_composite([
                    ("ref_time", ScaleValue::u128(0)),
                    ("proof_size", ScaleValue::u128(0)),
                ]),
            ),
            (
                "class",
                ScaleValue::variant("Normal", Composite::Unnamed(vec![])),
            ),
            (
                "pays_fee",
                ScaleValue::variant("Yes", Composite::Unnamed(vec![])),
            ),
        ]);
        let event = if success {
            ScaleValue::variant(
                "ExtrinsicSuccess",
                Composite::Named(vec![("dispatch_info".into(), dispatch_info)]),
            )
        } else {
            ScaleValue::variant(
                "ExtrinsicFailed",
                Composite::Named(vec![
                    (
                        "dispatch_error".into(),
                        ScaleValue::variant("BadOrigin", Composite::Unnamed(vec![])),
                    ),
                    ("dispatch_info".into(), dispatch_info),
                ]),
            )
        };
        let record = ScaleValue::named_composite([
            (
                "phase",
                ScaleValue::variant(
                    "ApplyExtrinsic",
                    Composite::Unnamed(vec![ScaleValue::u128(0)]),
                ),
            ),
            (
                "event",
                ScaleValue::variant("System", Composite::Unnamed(vec![event])),
            ),
            (
                "topics",
                ScaleValue::unnamed_composite(Vec::<ScaleValue>::new()),
            ),
        ]);
        let at = self
            .runtime
            .block_on(self.client.at_current_block())
            .unwrap();
        let metadata = at.metadata();
        let events_type = metadata
            .pallet_by_name("System")
            .unwrap()
            .storage()
            .unwrap()
            .entry_by_name("Events")
            .unwrap()
            .value_ty();
        let mut bytes = Vec::new();
        encode_as_type(
            &ScaleValue::unnamed_composite(vec![record]),
            events_type,
            metadata.types(),
            &mut bytes,
        )
        .unwrap();
        let key = self.storage_key("System", "Events", vec![]);
        self.cassette
            .set_storage(&key, json!(format!("0x{}", hex::encode(bytes))));
    }

    pub fn set_subtensor(&self, name: &str, extra_keys: Vec<ScaleValue>, value: impl Encode) {
        let mut keys = vec![ScaleValue::u128(46)];
        keys.extend(extra_keys);
        let key = self.storage_key("SubtensorModule", name, keys);
        self.cassette
            .set_storage(&key, json!(format!("0x{}", hex::encode(value.encode()))));
    }
}
