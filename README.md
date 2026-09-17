# sn46-validator

SN46 validator for Linux x86_64 (Ubuntu 22.04+).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Subnet46/sn46-validator/main/install.sh | bash
```

## Run

```bash
sn46-validator run \
  --wallet-name YOUR_WALLET \
  --wallet-hotkey YOUR_HOTKEY \
  --platform-epoch-summary-url https://YOUR_PLATFORM/validator/v1/epoch-summaries/latest \
  --platform-signer PLATFORM_SS58
```

- `run` — keep running.
- `run-once` — process one summary and exit; accepts the same options.
- `--help` — show all options.

Defaults to Finney, subnet 46. Use `--network`, `--netuid`, and `--chain-endpoint` for localnet.
