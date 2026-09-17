//! Subtensor v4 timelocked weights, using the chain's timelock engine.
//!
//! Payload, Quicknet parameters, and epoch scheduling follow bittensor-drand:
//! https://github.com/opentensor/bittensor-drand/tree/36a729139c34af26cd88c2d2d03097b99b07cd45/src

use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use subtensor::api::{self, runtime_types::subtensor_runtime_common::NetUid};
use subxt::ext::codec::Encode;
use tle::{
    curves::drand::TinyBLS381, ibe::fullident::Identity,
    stream_ciphers::AESGCMStreamCipherProvider, tlock::tle,
};
use w3f_bls::EngineBLS;

use crate::burn::BurnError;
use crate::chain::{At, read_tempo};

const QUICKNET_KEY: &str = "83cf0f2896adee7eb8b5f01fcad3912212c437e0073e911fb90022d3e760183c8c4b450b6a0a6c3ac6a5776a2d1064510d1fec758c921cc22b0e17e63aaf4bcb5ed66304de9cf809bd274ca73bab4af5a6e9c76a4bc09e76eae8991ef5ece45a";
const QUICKNET_GENESIS: u64 = 1_692_803_367;
const MAX_TEMPO: u64 = 50_400;

fn error(error: impl std::fmt::Display) -> BurnError {
    BurnError(format!("Commit/reveal preparation failed: {error}"))
}

#[derive(Clone)]
struct Schedule {
    block: u64,
    last_epoch: u64,
    pending_epoch: u64,
    epoch: u64,
    tempo: u16,
    blocks_since_step: u64,
}

impl Schedule {
    fn epoch_due(&self, block: u64) -> bool {
        (self.pending_epoch > 0 && block >= self.pending_epoch)
            || self.blocks_since_step > MAX_TEMPO
            || block.saturating_sub(self.last_epoch) >= u64::from(self.tempo)
    }

    fn reveal_block(mut self, period: u64) -> Result<u64, BurnError> {
        if self.tempo == 0 || !(1..=100).contains(&period) {
            return Err(error("invalid tempo or reveal period"));
        }
        let first = self
            .block
            .checked_add(1)
            .ok_or_else(|| error("block overflow"))?;
        let target = self
            .epoch
            .checked_add(u64::from(self.epoch_due(first)))
            .and_then(|epoch| epoch.checked_add(period))
            .ok_or_else(|| error("epoch overflow"))?;
        let end = first
            .checked_add((period + 1) * MAX_TEMPO)
            .ok_or_else(|| error("block overflow"))?;
        for block in first..=end {
            // Reveals run before coinbase, with a one-epoch lookahead.
            if self.epoch.saturating_add(u64::from(self.epoch_due(block))) == target {
                return Ok(block);
            }
            self.blocks_since_step = self.blocks_since_step.saturating_add(1);
            if self.epoch_due(block) {
                self.last_epoch = block;
                self.pending_epoch = 0;
                self.epoch = self.epoch.saturating_add(1);
                self.blocks_since_step = 0;
            }
        }
        Err(error("reveal block exceeds simulation bound"))
    }
}

pub(crate) async fn encrypted_weights(
    at: &At,
    netuid: u16,
    hotkey: &[u8; 32],
    uids: Vec<u16>,
    weights: Vec<u16>,
    version: u64,
) -> Result<(Vec<u8>, u64), BurnError> {
    let storage = at.storage();
    let module = api::storage().subtensor_module();
    let protocol: u16 = storage
        .fetch(module.commit_reveal_weights_version(), ())
        .await
        .map_err(error)?
        .decode()
        .map_err(error)?;
    if protocol != 4 {
        return Err(error(format!(
            "unsupported commit/reveal version {protocol}"
        )));
    }
    let keys = || (NetUid(netuid),);
    let schedule = Schedule {
        block: at.block_number(),
        last_epoch: storage
            .fetch(module.last_epoch_block(), keys())
            .await
            .map_err(error)?
            .decode()
            .map_err(error)?,
        pending_epoch: storage
            .fetch(module.pending_epoch_at(), keys())
            .await
            .map_err(error)?
            .decode()
            .map_err(error)?,
        epoch: storage
            .fetch(module.subnet_epoch_index(), keys())
            .await
            .map_err(error)?
            .decode()
            .map_err(error)?,
        tempo: read_tempo(at, netuid).await.map_err(error)?,
        blocks_since_step: storage
            .fetch(module.blocks_since_last_step(), keys())
            .await
            .map_err(error)?
            .decode()
            .map_err(error)?,
    };
    let period: u64 = storage
        .fetch(module.reveal_period_epochs(), keys())
        .await
        .map_err(error)?
        .decode()
        .map_err(error)?;
    let block = schedule.block;
    let reveal_block = schedule.reveal_block(period)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(error)?
        .as_secs();
    // Match the SDK: 12s blocks, three security blocks, Quicknet's 3s rounds.
    let target = now
        .checked_add((reveal_block - block + 3) * 12)
        .ok_or_else(|| error("reveal time overflow"))?;
    let round = (target.saturating_sub(QUICKNET_GENESIS) / 3).max(1);
    let payload = (hotkey.to_vec(), uids, weights, version).encode();
    Ok((encrypt(&payload, round)?, round))
}

fn encrypt(payload: &[u8], round: u64) -> Result<Vec<u8>, BurnError> {
    let key = hex::decode(QUICKNET_KEY).map_err(error)?;
    let key =
        <TinyBLS381 as EngineBLS>::PublicKeyGroup::deserialize_compressed(&*key).map_err(error)?;
    let identity = Identity::new(b"", vec![Sha256::digest(round.to_be_bytes()).to_vec()]);
    let mut secret = [0; 32];
    OsRng.fill_bytes(&mut secret);
    let ciphertext =
        tle::<TinyBLS381, AESGCMStreamCipherProvider, OsRng>(key, secret, payload, identity, OsRng)
            .map_err(|cause| error(format!("encryption: {cause:?}")))?;
    let mut bytes = Vec::new();
    ciphertext.serialize_compressed(&mut bytes).map_err(error)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use subxt::ext::codec::Decode;
    use tle::tlock::{TLECiphertext, tld};

    #[test]
    fn reveal_schedule_covers_epoch_boundaries_pending_epochs_and_overdue_steps() {
        let state = Schedule {
            block: 722,
            last_epoch: 720,
            pending_epoch: 0,
            epoch: 2,
            tempo: 360,
            blocks_since_step: 2,
        };
        assert_eq!(state.clone().reveal_block(1).unwrap(), 1080);
        assert_eq!(state.clone().reveal_block(2).unwrap(), 1440);
        assert_eq!(
            Schedule {
                block: 1079,
                ..state.clone()
            }
            .reveal_block(1)
            .unwrap(),
            1440
        );
        assert_eq!(
            Schedule {
                pending_epoch: 730,
                ..state.clone()
            }
            .reveal_block(1)
            .unwrap(),
            730
        );
        assert_eq!(
            Schedule {
                pending_epoch: 723,
                ..state.clone()
            }
            .reveal_block(1)
            .unwrap(),
            1083
        );
        assert_eq!(
            Schedule {
                blocks_since_step: MAX_TEMPO + 1,
                ..state.clone()
            }
            .reveal_block(1)
            .unwrap(),
            1083
        );
        assert!(
            Schedule {
                tempo: 0,
                ..state.clone()
            }
            .reveal_block(1)
            .is_err()
        );
        assert!(state.reveal_block(0).is_err());
    }

    #[test]
    fn chain_ciphertext_decrypts_to_the_scale_weight_payload() {
        // Public Quicknet round 17200000; no network needed for this regression.
        let signature = hex::decode("9672ff8379fd8339523ab38b8c79637b92dca07988352f7a2c1abd3ec8b4672a2f7a61ca8984b4051b7c9fc66c6ee4db").unwrap();
        let signature =
            <TinyBLS381 as EngineBLS>::SignatureGroup::deserialize_compressed(&*signature).unwrap();
        let payload = (vec![1u8; 32], vec![12u16, 238], vec![123u16, 65535], 62u64);
        let encrypted = encrypt(&payload.encode(), 17_200_000).unwrap();
        let ciphertext = TLECiphertext::<TinyBLS381>::deserialize_compressed(&*encrypted).unwrap();
        let plaintext =
            tld::<TinyBLS381, AESGCMStreamCipherProvider>(ciphertext, signature).unwrap();
        assert_eq!(
            <(Vec<u8>, Vec<u16>, Vec<u16>, u64)>::decode(&mut &*plaintext).unwrap(),
            payload
        );
        let later = encrypt(&payload.encode(), 17_200_001).unwrap();
        let later = TLECiphertext::<TinyBLS381>::deserialize_compressed(&*later).unwrap();
        assert!(tld::<TinyBLS381, AESGCMStreamCipherProvider>(later, signature).is_err());
    }
}
