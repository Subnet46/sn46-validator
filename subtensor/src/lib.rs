//! Typed storage addresses and runtime types generated from finney's metadata (spec 455,
//! fetched 2026-09-10). Refresh `metadata.scale` after a runtime upgrade with
//! `subxt metadata --url wss://entrypoint-finney.opentensor.ai:443 -f bytes > subtensor/metadata.scale`.

#[subxt::subxt(runtime_metadata_path = "metadata.scale")]
pub mod api {}
