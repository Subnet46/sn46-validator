use std::io::IsTerminal;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, CommandFactory, Parser, ValueEnum};
use sn46_validator::burn::BittensorBurnWriter;
use sn46_validator::chain::BittensorChain;
use sn46_validator::runtime::{Run, RunError, fetch_epoch_summary, run_once};
use sn46_validator::state::StateStore;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;

const PLATFORM_URL: &str = "http://107.170.30.202/validator/v1/epoch-summaries/latest";
const PLATFORM_SIGNER: &str = "5FutpWD5tJHoqaX3DaDwiqn2isxmZ19VeRECRd6vgSif4moZ";

#[derive(Parser)]
#[command(version, about = "Validate and score signed Platform summaries")]
struct Cli {
    /// Run continuously, or process one summary and exit.
    #[arg(value_enum, default_value = "run")]
    command: Command,
    #[command(flatten)]
    options: Options,
}

#[derive(Clone, Copy, Default, ValueEnum)]
enum Command {
    /// Poll for summaries until SIGINT or SIGTERM (the default).
    #[default]
    Run,
    /// Process the current summary and exit.
    RunOnce,
}

#[derive(Args)]
struct Options {
    /// Trusted platform SS58 address.
    #[arg(long, env = "PLATFORM_SIGNER", default_value = PLATFORM_SIGNER)]
    platform_signer: Option<String>,
    /// Chain network: finney, test, or local.
    #[arg(long, env = "NETWORK", default_value = "finney")]
    network: String,
    /// Subnet ID (1 through 65535).
    #[arg(long, env = "NETUID", default_value = "46")]
    netuid: NonZeroU16,
    /// Override the network's chain endpoint.
    #[arg(long, env = "CHAIN_ENDPOINT", default_value = "")]
    chain_endpoint: String,
    /// URL serving the latest signed platform summary.
    #[arg(
        long = "platform-epoch-summary-url",
        env = "PLATFORM_EPOCH_SUMMARY_URL",
        default_value = PLATFORM_URL
    )]
    platform_epoch_summary_url: Option<String>,
    /// File storing processed summaries and finalized burns.
    #[arg(
        long,
        env = "VALIDATOR_STATE_PATH",
        default_value = "/var/lib/sn46-validator/state.json"
    )]
    state_path: PathBuf,
    /// Seconds to wait after each attempt.
    #[arg(long, env = "POLL_INTERVAL_SECS", default_value = "300", value_parser = clap::value_parser!(u64).range(1..=86400))]
    poll_interval_secs: u64,
    /// Maximum seconds for a weight submission, including finalization.
    #[arg(long, env = "TRANSACTION_TIMEOUT_SECS", default_value = "60", value_parser = clap::value_parser!(u64).range(1..=3600))]
    transaction_timeout_secs: u64,
    /// Wallet directory name within the wallet path.
    #[arg(long, env = "WALLET_NAME", default_value = "validator")]
    wallet_name: String,
    /// Hotkey filename within the wallet's hotkeys directory.
    #[arg(long, env = "WALLET_HOTKEY", default_value = "default")]
    wallet_hotkey: String,
    /// Directory containing wallets; a leading ~ expands to HOME.
    #[arg(long, env = "WALLET_PATH", default_value = "~/.bittensor/wallets")]
    wallet_path: PathBuf,
    /// Tracing filter.
    #[arg(long, env = "LOG", default_value = "info")]
    log: String,
}

fn expand_user(path: PathBuf) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path,
    }
}

fn configuration_error(message: &str) -> clap::Error {
    Cli::command().error(clap::error::ErrorKind::ValueValidation, message)
}

struct Config {
    network: String,
    netuid: NonZeroU16,
    endpoint: String,
    epoch_summary_url: String,
    platform_signer: String,
    state: StateStore,
    poll_interval: Duration,
    transaction_timeout: Duration,
    wallet_name: String,
    wallet_hotkey: String,
    wallet_path: PathBuf,
}

impl Config {
    fn from_options(options: Options) -> Result<Self, clap::Error> {
        let platform_signer = options
            .platform_signer
            .filter(|signer| !signer.is_empty())
            .ok_or_else(|| {
                configuration_error("PLATFORM_SIGNER is required (or pass --platform-signer)")
            })?;
        let epoch_summary_url = options
            .platform_epoch_summary_url
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                configuration_error(
                    "PLATFORM_EPOCH_SUMMARY_URL is required (or pass --platform-epoch-summary-url)",
                )
            })?;
        Ok(Self {
            network: options.network,
            netuid: options.netuid,
            endpoint: options.chain_endpoint,
            epoch_summary_url,
            platform_signer,
            state: StateStore::new(options.state_path),
            poll_interval: Duration::from_secs(options.poll_interval_secs),
            transaction_timeout: Duration::from_secs(options.transaction_timeout_secs),
            wallet_name: options.wallet_name,
            wallet_hotkey: options.wallet_hotkey,
            wallet_path: expand_user(options.wallet_path),
        })
    }

    fn run_once(&self) -> Result<(), RunError> {
        // Reconnect each cycle so a dropped node connection is retried on the next poll.
        let chain = BittensorChain::connect(&self.network, &self.endpoint)?;
        let burner = BittensorBurnWriter::new(
            &chain,
            &self.wallet_name,
            &self.wallet_hotkey,
            &self.wallet_path,
            &self.state.path,
        )?
        .with_timeout(self.transaction_timeout);
        run_once(&Run {
            network: &self.network,
            netuid: u64::from(self.netuid.get()),
            epoch_summary_url: &self.epoch_summary_url,
            platform_signer: &self.platform_signer,
            chain: &chain,
            state: &self.state,
            burner: &burner,
            timeout: Duration::from_secs(15),
            fetch: &fetch_epoch_summary,
        })?;
        Ok(())
    }
}

fn log_outcome(outcome: &Result<(), RunError>) {
    if let Err(error) = outcome {
        tracing::error!(error = %error, "❌ Validator run failed");
    }
}

async fn serve(config: Config) -> std::io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let config = Arc::new(config);
    loop {
        let worker_config = Arc::clone(&config);
        // The existing chain and HTTP clients are synchronous; keep them off the signal runtime.
        let mut attempt = tokio::task::spawn_blocking(move || worker_config.run_once());
        let stopping = tokio::select! {
            outcome = &mut attempt => {
                log_outcome(&outcome?);
                false
            }
            _ = interrupt.recv() => true,
            _ = terminate.recv() => true,
        };
        if stopping {
            tracing::info!("🛑 Shutdown requested; finishing the current run");
            log_outcome(&attempt.await?);
            return Ok(());
        }
        tracing::info!("Next check in {}s", config.poll_interval.as_secs());
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {},
            _ = interrupt.recv() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
        }
    }
}

fn log_time(writer: &mut Writer<'_>) -> std::fmt::Result {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 86400;
    write!(
        writer,
        "{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = EnvFilter::try_new(&cli.options.log).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .with_timer(log_time as fn(&mut Writer<'_>) -> std::fmt::Result)
        .with_ansi(std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
        .init();
    let config = Config::from_options(cli.options).unwrap_or_else(|error| error.exit());
    let mode = if matches!(cli.command, Command::RunOnce) {
        "run-once"
    } else {
        "run"
    };
    tracing::info!(
        "🚀 Validator started mode={mode} network={} subnet={} wallet={}/{}",
        config.network,
        config.netuid,
        config.wallet_name,
        config.wallet_hotkey
    );
    if matches!(cli.command, Command::RunOnce) {
        let outcome = config.run_once();
        log_outcome(&outcome);
        return if outcome.is_ok() {
            tracing::info!("Run complete");
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| runtime.block_on(serve(config)));
    match result {
        Ok(()) => {
            tracing::info!("🛑 Validator stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(error = %error, "❌ Validator stopped");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_mainnet_with_the_trusted_do_platform() {
        let cli = Cli::try_parse_from(["sn46-validator"]).unwrap();
        let config = Config::from_options(cli.options).unwrap();
        assert_eq!(config.network, "finney");
        assert_eq!(config.netuid.get(), 46);
        assert!(config.endpoint.is_empty());
        assert_eq!(config.epoch_summary_url, PLATFORM_URL);
        assert_eq!(config.platform_signer, PLATFORM_SIGNER);
    }
}
