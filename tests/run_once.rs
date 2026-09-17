//! Validator CLI end to end: continuous and single runs, flag/environment configuration,
//! shutdown, and mandatory burns against a fake chain and local summary server.

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use subxt::dynamic::Value;
use support::Node;
use support::fake_node::{Cassette, FakeNode};

const CASSETTE: &str = "finney_46_9036625.json";
const FIXTURE: &str = include_str!("fixtures/epoch_summary_v2.json");
const SIGNER: &str = "5DAAnrj7VHTznn2AWBemMuyBwZWs6FNFjdyVXUeYum3PTXFy";
const DEVELOPMENT_HOTKEY: &str = "5FnoWRL4FYzd29q8zXoY3JWio9PtBPgrAFu2RcgQazRQsiCs";
const FINALIZED: u64 = 722;

/// Serves the fixture summary to every GET, forever, on a background thread.
fn epoch_summary_server(body: &'static str) -> String {
    epoch_summary_server_with(move |_| body.to_owned())
}

fn epoch_summary_server_with(mut body: impl FnMut(usize) -> String + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/validator/v1/epoch-summaries/latest",
        listener.local_addr().unwrap()
    );
    std::thread::spawn(move || {
        for (request_number, stream) in listener.incoming().enumerate() {
            let mut stream = stream.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = body(request_number);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    url
}

/// The finney cassette rewritten to the fixture's epoch: finalized block 722, last step
/// 720, 56 UIDs with the fixture's miners in theirs and the development hotkey at UID 0.
fn epoch() -> Node {
    let epoch = Node::new(Cassette::load(CASSETTE));
    let hash = epoch.cassette.block_hash.clone();
    let mut header = epoch
        .cassette
        .get("chain_getHeader", json!([hash]))
        .unwrap();
    header["number"] = json!(format!("{FINALIZED:#x}"));
    epoch.cassette.set("chain_getHeader", json!([hash]), header);
    epoch
        .cassette
        .set("chain_getBlockHash", json!([FINALIZED]), json!(hash));
    epoch.set_subtensor("SubnetworkN", vec![], 56u16);
    epoch.set_subtensor("LastMechansimStepBlock", vec![], 720u64);
    epoch.set_subtensor("BlocksSinceLastStep", vec![], 2u64);
    epoch.set_subtensor("LastUpdate", vec![], vec![0u64; 56]);
    let epoch_summary: Json = serde_json::from_str(FIXTURE).unwrap();
    for row in epoch_summary["miners"].as_array().unwrap() {
        let (key, _) =
            sn46_shared::identity::decode_registered_ss58(row["hotkey"].as_str().unwrap()).unwrap();
        epoch.set_subtensor(
            "Keys",
            vec![Value::u128(row["uid"].as_u64().unwrap().into())],
            key,
        );
    }
    let (development, _) =
        sn46_shared::identity::decode_registered_ss58(DEVELOPMENT_HOTKEY).unwrap();
    epoch.set_subtensor("Keys", vec![Value::u128(0)], development);
    epoch.cassette.set(
        "system_accountNextIndex",
        json!([DEVELOPMENT_HOTKEY]),
        json!(3),
    );
    epoch
}

struct Outcome {
    code: i32,
    stderr: String,
}

impl Outcome {
    fn lines(&self) -> Vec<&str> {
        self.stderr.lines().collect()
    }
    fn messages(&self) -> Vec<String> {
        // Strip the timestamp and level prefix; the message text is what parity covers.
        self.lines()
            .into_iter()
            .filter_map(|line| {
                line.split_once(" INFO ")
                    .or_else(|| line.split_once(" ERROR "))
            })
            .map(|(_, message)| message.trim().to_owned())
            .collect()
    }
}

fn run_once(env: &[(&str, &str)]) -> Outcome {
    run(&["run-once"], env)
}

fn run(args: &[&str], env: &[(&str, &str)]) -> Outcome {
    let output = Command::new(env!("CARGO_BIN_EXE_sn46-validator"))
        .args(args)
        .env_clear()
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .envs(env.iter().copied())
        .output()
        .unwrap();
    Outcome {
        code: output.status.code().unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn wallet(temp: &tempfile::TempDir) -> String {
    let hotkeys = temp.path().join("wallets/validator/hotkeys");
    std::fs::create_dir_all(&hotkeys).unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/development_hotkey.json"
        ),
        hotkeys.join("default"),
    )
    .unwrap();
    temp.path().join("wallets").to_str().unwrap().to_owned()
}

#[test]
fn cli_requires_platform_signer_before_chain_contact() {
    let outcome = run_once(&[]);
    assert_eq!(outcome.code, 2);
    assert!(outcome.stderr.contains("PLATFORM_SIGNER is required"));
    for netuid in ["0", "-1", "65536", "18446744073709551616", "nope"] {
        let outcome = run_once(&[("NETUID", netuid), ("PLATFORM_SIGNER", SIGNER)]);
        assert_eq!(outcome.code, 2, "{netuid}");
        assert!(outcome.stderr.contains("--netuid"));
    }
    for netuid in ["1", "65535"] {
        let outcome = run_once(&[
            ("NETUID", netuid),
            ("PLATFORM_SIGNER", SIGNER),
            ("NETWORK", "unknown"),
            (
                "PLATFORM_EPOCH_SUMMARY_URL",
                "http://127.0.0.1:8092/validator/v1/epoch-summaries/latest",
            ),
        ]);
        assert_eq!(outcome.code, 1);
        assert!(outcome.stderr.contains("unknown network: unknown"));
    }
    for args in [vec![], vec!["other"], vec!["run-once", "extra"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_sn46-validator"))
            .args(args)
            .env_clear()
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}

#[test]
fn cli_requires_summary_url_before_chain_contact() {
    for extra in [
        vec![],
        vec![("PLATFORM_EPOCH_SUMMARY_URL", "")],
        vec![("PLATFORM_EPOCH_SUMMARY_URL", " \t ")],
    ] {
        let mut env = vec![("PLATFORM_SIGNER", SIGNER), ("NETWORK", "unknown")];
        env.extend(extra);
        let outcome = run_once(&env);
        assert_eq!(outcome.code, 2);
        assert!(
            outcome
                .stderr
                .contains("PLATFORM_EPOCH_SUMMARY_URL is required")
        );
        assert!(outcome.stderr.contains("--platform-epoch-summary-url"));
    }
}

#[test]
fn run_once_processes_the_fixture_epoch_end_to_end() {
    let epoch = epoch();
    let epoch_summary_url = epoch_summary_server(FIXTURE.trim_end_matches('\n'));
    let temp = tempfile::tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let wallet_path = wallet(&temp);
    let env = [
        ("NETWORK", "local"),
        ("NETUID", "46"),
        ("CHAIN_ENDPOINT", epoch.node.url.as_str()),
        ("PLATFORM_EPOCH_SUMMARY_URL", epoch_summary_url.as_str()),
        ("PLATFORM_SIGNER", SIGNER),
        ("VALIDATOR_STATE_PATH", state_path.to_str().unwrap()),
        ("WALLET_PATH", &wallet_path),
    ];

    let store = sn46_validator::state::StateStore::new(&state_path);
    let lock = store.lock().unwrap();
    let locked = run_once(&env);
    assert_eq!(locked.code, 1);
    assert!(
        locked
            .stderr
            .contains("Cannot acquire validator state lock")
    );
    assert!(!state_path.exists());
    drop(lock);

    epoch.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    epoch.set_subtensor("RecycleOrBurn", vec![], 1u8);
    let first = run_once(&env);
    // Scoring is saved even when the chain refuses the mandatory burn.
    assert_eq!(first.code, 1, "{}", first.stderr);
    let messages = first.messages();
    assert_eq!(
        messages
            .iter()
            .filter(|line| line.starts_with("miner_score "))
            .count(),
        3
    );
    assert_eq!(
        messages[messages.len() - 2],
        "epoch_summary_processed summary_id=local-46-361-720 finalized_block=722 miners=3"
    );
    let golden: Json = serde_json::from_str(include_str!("golden/scoring_cases.json")).unwrap();
    let expected_lines = &golden.as_array().unwrap()[0]["log_lines"];
    let score_lines: Vec<Json> = messages
        .iter()
        .filter(|line| line.starts_with("miner_score "))
        .map(|line| json!(line))
        .collect();
    assert_eq!(Json::Array(score_lines), *expected_lines);
    let epoch_summary: Json = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(
        std::fs::read_to_string(&state_path).unwrap(),
        format!(
            "{{\"burn_epoch_end_block\":null,\"summary_digest\":\"{}\",\"summary_epoch_end_block\":720,\"summary_id\":\"local-46-361-720\"}}\n",
            epoch_summary["digest"].as_str().unwrap()
        )
    );

    // The saved summary is not rescored; only the failed burn is retried.
    let second = run_once(&env);
    assert_eq!(second.code, 1, "{}", second.stderr);
    assert_eq!(
        second.messages(),
        [
            "epoch_summary_already_processed summary_id=local-46-361-720",
            "Validator run failed error=Subnet is not in Burn mode",
        ]
    );
    assert!(
        std::fs::read_to_string(&state_path)
            .unwrap()
            .contains("\"burn_epoch_end_block\":null")
    );

    // Burn mode restored and the extrinsic succeeds: the exact burn_submitted line.
    epoch.set_subtensor("RecycleOrBurn", vec![], 0u8);
    epoch.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    epoch.set_events(true);
    let burned = run_once(&env);
    assert_eq!(burned.code, 0, "{}", burned.stderr);
    assert_eq!(
        burned.messages(),
        [
            "epoch_summary_already_processed summary_id=local-46-361-720",
            "burn_submitted epoch_end_block=720 result=finalized",
        ]
    );
    assert_eq!(epoch.node.submissions.lock().unwrap().len(), 1);
    assert!(
        std::fs::read_to_string(&state_path)
            .unwrap()
            .contains("\"burn_epoch_end_block\":720")
    );

    let again = run_once(&env);
    assert_eq!(again.code, 0);
    assert_eq!(
        again.messages(),
        ["epoch_summary_already_processed summary_id=local-46-361-720"]
    );
    assert_eq!(epoch.node.submissions.lock().unwrap().len(), 1);
}

#[test]
fn chain_mismatch_and_fetch_failures_exit_one() {
    let cassette = Cassette::load(CASSETTE);
    let node = FakeNode::spawn(cassette);
    let epoch_summary_url = epoch_summary_server(FIXTURE.trim_end_matches('\n'));
    let temp = tempfile::tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let wallet_path = wallet(&temp);
    let outcome = run_once(&[
        ("NETWORK", "local"),
        ("CHAIN_ENDPOINT", node.url.as_str()),
        ("PLATFORM_EPOCH_SUMMARY_URL", epoch_summary_url.as_str()),
        ("PLATFORM_SIGNER", SIGNER),
        ("VALIDATOR_STATE_PATH", state_path.to_str().unwrap()),
        ("WALLET_PATH", &wallet_path),
    ]);
    assert_eq!(outcome.code, 1);
    assert_eq!(
        outcome.messages(),
        ["Validator run failed error=summary is not the latest finalized chain epoch"]
    );
    assert!(!state_path.exists());
    let outcome = run_once(&[
        ("NETWORK", "local"),
        ("CHAIN_ENDPOINT", node.url.as_str()),
        ("PLATFORM_EPOCH_SUMMARY_URL", "http://127.0.0.1:1/latest"),
        ("PLATFORM_SIGNER", SIGNER),
        ("VALIDATOR_STATE_PATH", state_path.to_str().unwrap()),
        ("WALLET_PATH", &wallet_path),
    ]);
    assert_eq!(outcome.code, 1);
    assert!(
        outcome
            .stderr
            .contains("Validator run failed error=Platform summary fetch failed: "),
        "{}",
        outcome.stderr
    );
}

struct Running {
    child: Child,
    log: PathBuf,
}

impl Running {
    fn start(
        temp: &tempfile::TempDir,
        node: &Node,
        epoch_summary_url: &str,
        args: &[&str],
    ) -> Self {
        let log = temp.path().join("validator.log");
        let child = Command::new(env!("CARGO_BIN_EXE_sn46-validator"))
            .args(args)
            .env_clear()
            .env("HOME", temp.path())
            .env(
                "VALIDATOR_STATE_PATH",
                temp.path().join("state/sn46-validator/state.json"),
            )
            .env("PLATFORM_SIGNER", SIGNER)
            .env("NETWORK", "local")
            .env("CHAIN_ENDPOINT", &node.node.url)
            .env("PLATFORM_EPOCH_SUMMARY_URL", epoch_summary_url)
            .env("POLL_INTERVAL_SECS", "1")
            .env("WALLET_PATH", wallet(temp))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self { child, log }
    }

    fn messages(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap()
    }

    fn wait_for(&mut self, message: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.messages().contains(message) {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "{}",
                self.messages()
            );
            assert!(
                Instant::now() < deadline,
                "waiting for {message}: {}",
                self.messages()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, signal: &str) {
        assert!(
            Command::new("kill")
                .arg(signal)
                .arg(self.child.id().to_string())
                .status()
                .unwrap()
                .success()
        );
    }

    fn wait_for_success(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{}", self.messages());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "validator did not stop: {}",
                self.messages()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn continuous_mode_retries_failures_without_rescoring_and_stops_on_interrupt() {
    let node = epoch();
    node.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    node.set_events(true);
    let epoch_summary_url = epoch_summary_server_with(|request| {
        if request == 0 {
            "invalid json"
        } else {
            FIXTURE.trim_end_matches('\n')
        }
        .to_owned()
    });
    let temp = tempfile::tempdir().unwrap();
    // No arguments starts continuous mode; the environment overrides the state path.
    let mut validator = Running::start(&temp, &node, &epoch_summary_url, &[]);
    validator.wait_for("Validator run failed");
    validator.wait_for("epoch_summary_already_processed");
    validator.signal("-INT");
    validator.wait_for_success();
    let messages = validator.messages();
    assert_eq!(messages.matches("miner_score ").count(), 3);
    assert_eq!(messages.matches("epoch_summary_processed ").count(), 1);
    assert_eq!(messages.matches("burn_submitted ").count(), 1);
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    assert!(
        temp.path()
            .join("state/sn46-validator/state.json")
            .is_file()
    );
}

#[test]
fn termination_finishes_an_in_flight_run_before_exiting() {
    let node = epoch();
    node.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    node.set_events(true);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let epoch_summary_url = epoch_summary_server_with(move |_| {
        started_tx.send(()).unwrap();
        finish_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        FIXTURE.trim_end_matches('\n').to_owned()
    });
    let temp = tempfile::tempdir().unwrap();
    let mut validator = Running::start(&temp, &node, &epoch_summary_url, &["run"]);
    started_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    validator.signal("-TERM");
    validator.wait_for("Shutdown requested");
    assert!(validator.child.try_wait().unwrap().is_none());
    finish_tx.send(()).unwrap();
    validator.wait_for_success();
    assert!(validator.messages().contains("epoch_summary_processed "));
    let state =
        sn46_validator::state::StateStore::new(temp.path().join("state/sn46-validator/state.json"));
    assert_eq!(state.load().unwrap().summary_epoch_end_block, Some(720));
    assert_eq!(state.load().unwrap().burn_epoch_end_block, Some(720));
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    assert!(state.lock().is_ok());
}

#[test]
fn cli_help_version_and_poll_interval_validation_need_no_node() {
    for flag in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_sn46-validator"))
            .arg(flag)
            .env_clear()
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("sn46-validator"));
        assert!(!stdout.contains("--burn-enabled"));
        assert!(!stdout.contains("BURN_ENABLED"));
        assert!(!stdout.contains("SN46_"));
        if flag == "--help" {
            assert!(stdout.contains("/var/lib/sn46-validator/state.json"));
        }
    }
    for seconds in ["0", "-1", "86401", "18446744073709551616", "oops"] {
        let outcome = run_once(&[("PLATFORM_SIGNER", SIGNER), ("POLL_INTERVAL_SECS", seconds)]);
        assert_eq!(outcome.code, 2);
        assert!(outcome.stderr.contains("--poll-interval-secs"));
    }
}

#[test]
fn stalled_submission_stops_polls_and_reconciles_without_a_duplicate() {
    use std::sync::atomic::Ordering;
    let node = epoch();
    node.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    node.node.stall_submissions.store(true, Ordering::Relaxed);
    let epoch_summary_url = epoch_summary_server(FIXTURE.trim_end_matches('\n'));
    let temp = tempfile::tempdir().unwrap();
    let args = ["--transaction-timeout-secs", "1"];
    let mut validator = Running::start(&temp, &node, &epoch_summary_url, &args);
    let deadline = Instant::now() + Duration::from_secs(10);
    while node.node.submissions.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "{}", validator.messages());
        std::thread::sleep(Duration::from_millis(10));
    }
    validator.signal("-TERM");
    validator.wait_for_success();
    assert!(validator.messages().contains("Burn submission timed out"));
    let state =
        sn46_validator::state::StateStore::new(temp.path().join("state/sn46-validator/state.json"));
    assert_eq!(state.load().unwrap().burn_epoch_end_block, None);
    drop(validator);

    let mut validator = Running::start(&temp, &node, &epoch_summary_url, &args);
    validator.wait_for("Previous submission is uncertain");
    // The transaction finalized while the validator was stopped. LastUpdate is
    // sufficient for both direct weights and timelocked commits on this runtime.
    let mut updates = vec![0u64; 56];
    updates[0] = FINALIZED;
    node.set_subtensor("LastUpdate", vec![], updates);
    validator.wait_for("burn_already_on_chain");
    validator.signal("-TERM");
    validator.wait_for_success();
    assert_eq!(state.load().unwrap().burn_epoch_end_block, Some(720));
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
}

#[test]
fn cli_flags_override_environment_on_either_side_of_the_command() {
    let node = epoch();
    node.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    node.set_events(true);
    let epoch_summary_url = epoch_summary_server(FIXTURE.trim_end_matches('\n'));
    let temp = tempfile::tempdir().unwrap();
    let unused_state = temp.path().join("unused.json");
    let wallet_path = wallet(&temp);
    let environment = [
        ("NETWORK", "unknown"),
        ("CHAIN_ENDPOINT", "invalid"),
        ("NETUID", "0"),
        ("PLATFORM_EPOCH_SUMMARY_URL", "invalid"),
        ("PLATFORM_SIGNER", "wrong"),
        ("VALIDATOR_STATE_PATH", unused_state.to_str().unwrap()),
        ("POLL_INTERVAL_SECS", "0"),
        ("WALLET_PATH", "missing-wallet"),
        ("LOG", "off"),
    ];
    for command_first in [false, true] {
        let state = temp.path().join(format!("state-{command_first}.json"));
        let flags = [
            "--network",
            "local",
            "--chain-endpoint",
            &node.node.url,
            "--netuid",
            "46",
            "--platform-epoch-summary-url",
            &epoch_summary_url,
            "--platform-signer",
            SIGNER,
            "--state-path",
            state.to_str().unwrap(),
            "--poll-interval-secs",
            "1",
            "--wallet-path",
            &wallet_path,
            "--log",
            "info",
        ];
        let args: Vec<_> = if command_first {
            ["run-once"].into_iter().chain(flags).collect()
        } else {
            flags.into_iter().chain(["run-once"]).collect()
        };
        let outcome = run(&args, &environment);
        assert_eq!(outcome.code, 0, "{}", outcome.stderr);
        assert!(outcome.stderr.contains("epoch_summary_processed "));
        assert!(state.is_file());
    }
    assert!(!unused_state.exists());
    assert_eq!(node.node.submissions.lock().unwrap().len(), 2);
}

#[test]
fn cli_wallet_flags_expand_home_and_env_cannot_disable_burning() {
    let node = epoch();
    node.set_subtensor("CommitRevealWeightsEnabled", vec![], false);
    node.set_events(true);
    let epoch_summary_url = epoch_summary_server(FIXTURE.trim_end_matches('\n'));
    let temp = tempfile::tempdir().unwrap();
    let state_path = temp.path().join("state.json");
    let hotkeys = temp.path().join("wallets/custom/hotkeys");
    std::fs::create_dir_all(&hotkeys).unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/development_hotkey.json"
        ),
        hotkeys.join("custom-key"),
    )
    .unwrap();
    let outcome = run(
        &[
            "run-once",
            "--wallet-name",
            "custom",
            "--wallet-hotkey",
            "custom-key",
            "--wallet-path",
            "~/wallets",
        ],
        &[
            ("HOME", temp.path().to_str().unwrap()),
            ("VALIDATOR_STATE_PATH", state_path.to_str().unwrap()),
            ("NETWORK", "local"),
            ("CHAIN_ENDPOINT", &node.node.url),
            ("PLATFORM_EPOCH_SUMMARY_URL", &epoch_summary_url),
            ("PLATFORM_SIGNER", SIGNER),
            ("WALLET_NAME", "wrong"),
            ("WALLET_HOTKEY", "wrong"),
            ("WALLET_PATH", "wrong"),
            ("BURN_ENABLED", "false"),
        ],
    );
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stderr.contains("burn_submitted "));
    assert_eq!(node.node.submissions.lock().unwrap().len(), 1);
    let state = sn46_validator::state::StateStore::new(state_path);
    assert_eq!(state.load().unwrap().burn_epoch_end_block, Some(720));
}

#[test]
fn invalid_flags_fail_before_chain_contact() {
    let outcome = run_once(&[("PLATFORM_SIGNER", SIGNER), ("VALIDATOR_STATE_PATH", "")]);
    assert_eq!(outcome.code, 2);
    assert!(outcome.stderr.contains("--state-path"));
    for flag in [
        "--netuid=0",
        "--netuid=65536",
        "--poll-interval-secs=0",
        "--poll-interval-secs=86401",
        "--burn-enabled=true",
        "--burn-enabled=false",
        "--state-path=",
        "--platform-signer=",
    ] {
        let outcome = run(&["run-once", flag], &[("PLATFORM_SIGNER", SIGNER)]);
        assert_eq!(outcome.code, 2, "{flag}: {}", outcome.stderr);
    }
}
