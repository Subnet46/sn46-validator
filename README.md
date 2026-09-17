# sn46-validator

Rust validator for SN46. Authenticates signed platform epoch summaries, scores them,
and submits the current full owner-burn weights to Subtensor. Supports direct and
timelocked commit/reveal weights, including the existing runtime-393 localnet.

## Install

Linux x86_64, Ubuntu 22.04 or newer. No Rust or GitHub login is needed:

```bash
curl -fsSL https://raw.githubusercontent.com/Subnet46/sn46-validator/main/install.sh | bash
```

Installs the latest release to `/usr/local/bin/sn46-validator`, using sudo if needed.
Downloads are verified against the release's SHA-256 checksum. Run it again to update;
append `-s -- v0.1.0` after `bash` to select a version. For a custom destination,
use `| INSTALL_DIR="$HOME/.local/bin" bash`. The installer does not start a service.

## Configure and run

Use an existing registered validator wallet and your platform's summary URL and signer.
Download the example, edit it for your deployment, then export it:

```bash
curl -fsSLo .env.example https://raw.githubusercontent.com/Subnet46/sn46-validator/main/.env.example
cp -n .env.example .env
# Edit .env: PLATFORM_EPOCH_SUMMARY_URL, PLATFORM_SIGNER, wallet, and network settings.
set -a; . ./.env; set +a
sn46-validator
```

The binary does not load `.env` automatically. Defaults are `NETWORK=finney` and
`NETUID=46`; localnet needs `NETWORK=local`, its `NETUID`, and `CHAIN_ENDPOINT`.
Run as the wallet/state owner and set `VALIDATOR_STATE_PATH` to a writable location.
For the existing DO localnet, retain subnet 5 and `POLL_INTERVAL_SECS=1320` to respect
its 100-block weight rate limit. Use `sn46-validator --help` for all options.

Stop the previous validator before starting a replacement. Preserve both the state
file and its `.submission.json` journal; the default is `/var/lib/sn46-validator/state.json`.
An optional systemd template is in `deploy/sn46-validator.service`; configure its user
and `/etc/sn46-validator/localnet.env` before enabling it. The installer never changes
wallets, configuration, state, or services.

## Build and test

Source builds require Rust 1.98.1 for the release toolchain and GitHub read access to
the pinned private `Subnet46/sn46-shared` dependency. Binary users need neither.

```bash
gh auth setup-git
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --release --locked
cargo test --locked --workspace --all-targets
bash tests/install.sh
```

Tests require Docker for isolated localnet transactions; public chains are not modified.
The wallet fixture uses the public test-only seed `[0x42; 32]` and must never hold funds.
Pushing a version tag matching `Cargo.toml` runs checks and publishes the binary and
`SHA256SUMS`. Release builds use the `SN46_SHARED_READ_TOKEN` Actions secret, limited to Contents: read on `sn46-shared`.
