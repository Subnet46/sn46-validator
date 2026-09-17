use std::process::Command;
use std::time::Duration;

use subxt::rpcs::RpcClient;

// Subtensor's fast-runtime localnet starts both authorities needed for finalization.
const IMAGE: &str = "ghcr.io/raofoundation/subtensor-localnet@sha256:2307e340ecb187ad7ea8d2ac32f235e1f62facf2c2d261bf3f00764cd5f2c456";

pub struct Localnet {
    id: String,
    network: String,
    pub url: String,
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("localnet tests require Docker installed and running");
    assert!(
        output.status.success(),
        "docker {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Localnet {
    pub async fn start() -> (Self, RpcClient) {
        let name = tempfile::Builder::new()
            .prefix("sn46-localnet-")
            .tempdir()
            .unwrap();
        // Separate networks prevent parallel localnets from discovering each other's peers.
        // Install the guard before creating the container to clean up on startup failure.
        let mut node = Self {
            id: String::new(),
            url: String::new(),
            network: docker(&[
                "network",
                "create",
                name.path().file_name().unwrap().to_str().unwrap(),
            ]),
        };
        node.id = docker(&[
            "create",
            "--network",
            &node.network,
            "--publish",
            "127.0.0.1::9944",
            IMAGE,
        ]);
        docker(&["start", &node.id]);
        let address = docker(&["port", &node.id, "9944/tcp"]);
        let url = format!("ws://{address}");
        node.url = url.clone();
        let rpc = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                if let Ok(transport) = subxt::rpcs::client::jsonrpsee_client(&url).await {
                    let rpc = RpcClient::new(transport);
                    let methods = crate::support::legacy_methods(rpc.clone());
                    if let Ok(hash) = methods.chain_get_finalized_head().await
                        && let Ok(Some(header)) = methods.chain_get_header(Some(hash)).await
                        && header.number > 0
                    {
                        return rpc;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .expect("localnet did not finalize a block within 120 seconds");
        (node, rpc)
    }
}

impl Drop for Localnet {
    fn drop(&mut self) {
        if !self.id.is_empty() && std::thread::panicking() {
            // Preserve node diagnostics before deleting its ephemeral state.
            let _ = Command::new("docker")
                .args(["logs", "--tail", "80", &self.id])
                .status();
        }
        if !self.id.is_empty() {
            cleanup(&["rm", "--force", &self.id]);
        }
        cleanup(&["network", "rm", &self.network]);
    }
}

fn cleanup(args: &[&str]) {
    match Command::new("docker").args(args).output() {
        Ok(output) if output.status.success() => {}
        result => eprintln!(
            "localnet cleanup failed: docker {}: {result:?}",
            args.join(" ")
        ),
    }
}
