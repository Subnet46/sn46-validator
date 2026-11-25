//! What this validator can score, told to the platform on every summary fetch (D14).
//!
//! The `x-sn46-readiness` header is compact JSON signed with the validator hotkey: the summary
//! schema versions this build reads, the incentive documents it embeds (id and digest) and the
//! time it was made. The platform records the latest one per hotkey and publishes schema-v4
//! summaries, and signs new incentive documents, only once every validator it waits for says it
//! reads them. A header is good for a few minutes and only once: the platform takes a hotkey's
//! headers in strictly increasing `issued_at_ms`, so a copy replayed later records nothing.

use serde::Serialize;
use sn46_shared::epoch_summary::{SCHEMA_VERSION, SCHEMA_VERSION_V2, SCHEMA_VERSION_V4};

/// The request header the readiness travels in.
pub const HEADER: &str = "x-sn46-readiness";

/// What a readiness signature covers comes after these bytes, so it is never a summary's,
/// a record's or an extrinsic's signature.
pub const DOMAIN: &str = "sn46-readiness-v1:";

/// The summary schema versions this build scores.
pub const SUMMARY_VERSIONS: [u64; 3] = [SCHEMA_VERSION_V2, SCHEMA_VERSION, SCHEMA_VERSION_V4];

/// An incentive document this build embeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Accepted {
    pub digest: String,
    pub id: String,
}

/// The signed fields, in key order.
#[derive(Serialize)]
struct Payload<'a> {
    hotkey: &'a str,
    incentives: &'a [Accepted],
    issued_at_ms: u64,
    summary_versions: &'a [u64],
}

/// The header, in key order: the payload and its signature (lowercase hex, sr25519 by the
/// hotkey over [`DOMAIN`] and the payload's compact JSON).
#[derive(Serialize)]
struct Header<'a> {
    hotkey: &'a str,
    incentives: &'a [Accepted],
    issued_at_ms: u64,
    signature: String,
    summary_versions: &'a [u64],
}

/// The documents this build embeds, as the header names them.
pub fn accepted() -> Vec<Accepted> {
    sn46_shared::incentive::embedded()
        .iter()
        .filter_map(|id| sn46_shared::incentive::get(id).ok())
        .map(|document| Accepted {
            digest: document.digest,
            id: document.id,
        })
        .collect()
}

/// The bytes a header made at `issued_at_ms` by `hotkey` signs.
pub fn signing_payload(hotkey: &str, incentives: &[Accepted], issued_at_ms: u64) -> String {
    let payload = Payload {
        hotkey,
        incentives,
        issued_at_ms,
        summary_versions: &SUMMARY_VERSIONS,
    };
    // Plain strings and integers: serialising cannot fail.
    format!(
        "{DOMAIN}{}",
        serde_json::to_string(&payload).unwrap_or_default()
    )
}

/// The header value for `hotkey` at `issued_at_ms`, signed with `sign`.
pub fn header(hotkey: &str, issued_at_ms: u64, sign: impl FnOnce(&[u8]) -> [u8; 64]) -> String {
    let incentives = accepted();
    let signature = sign(signing_payload(hotkey, &incentives, issued_at_ms).as_bytes());
    serde_json::to_string(&Header {
        hotkey,
        incentives: &incentives,
        issued_at_ms,
        signature: hex::encode(signature),
        summary_versions: &SUMMARY_VERSIONS,
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::HotkeySigner;

    /// The header the platform's verifier is tested against (sn46-platform-rs
    /// `readiness::tests`): its payload bytes, and a signature that checks under the hotkey.
    #[test]
    fn the_header_is_signed_compact_json_the_platform_reads() {
        let raw = include_bytes!("../tests/fixtures/development_hotkey.json");
        let signer = HotkeySigner::from_json(raw).unwrap();
        let hotkey = signer.hotkey();
        let value = header(&hotkey, 1_786_708_800_000, |bytes| signer.sign_bytes(bytes));
        let parsed: serde_json::Value = serde_json::from_str(&value).unwrap();
        assert_eq!(
            parsed.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "hotkey",
                "incentives",
                "issued_at_ms",
                "signature",
                "summary_versions"
            ]
        );
        assert_eq!(parsed["summary_versions"], serde_json::json!([2, 3, 4]));
        let payload = signing_payload(&hotkey, &accepted(), 1_786_708_800_000);
        let relative = sn46_shared::incentive::get("relative-v1").unwrap().digest;
        let payout = sn46_shared::incentive::get("relative-v1-payout")
            .unwrap()
            .digest;
        assert_eq!(
            payload,
            format!(
                "sn46-readiness-v1:{{\"hotkey\":\"{hotkey}\",\"incentives\":[{{\"digest\":\"{relative}\",\"id\":\"relative-v1\"}},{{\"digest\":\"{payout}\",\"id\":\"relative-v1-payout\"}}],\"issued_at_ms\":1786708800000,\"summary_versions\":[2,3,4]}}"
            )
        );
        let signature: [u8; 64] = hex::decode(parsed["signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let (public, _) = sn46_shared::identity::decode_registered_ss58(&hotkey).unwrap();
        assert!(sn46_shared::identity::verify_sr25519(
            &public,
            payload.as_bytes(),
            &signature
        ));
        // Printed for the platform's copy of this fixture.
        println!("{value}");
    }

    /// `tests/fixtures/readiness_header.json` (the platform's `tests/fixtures` holds the same
    /// bytes and verifies them) is a header this build makes: its fields re-serialize to it
    /// and its signature covers this build's payload.
    #[test]
    fn the_shared_fixture_is_this_builds_header() {
        let raw = include_str!("../tests/fixtures/readiness_header.json").trim_end();
        let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
        let hotkey = parsed["hotkey"].as_str().unwrap();
        let issued_at_ms = parsed["issued_at_ms"].as_u64().unwrap();
        let signature = parsed["signature"].as_str().unwrap().to_owned();
        let remade = header(hotkey, issued_at_ms, |_| {
            hex::decode(&signature).unwrap().try_into().unwrap()
        });
        assert_eq!(remade, raw);
        let (public, _) = sn46_shared::identity::decode_registered_ss58(hotkey).unwrap();
        assert!(sn46_shared::identity::verify_sr25519(
            &public,
            signing_payload(hotkey, &accepted(), issued_at_ms).as_bytes(),
            &hex::decode(&signature).unwrap()
        ));
    }
}
