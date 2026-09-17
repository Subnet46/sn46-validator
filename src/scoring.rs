use sn46_shared::scoring::MinerScore;

/// Emit score records with keys in byte order, including flattened miner fields.
pub fn score_line(record: &MinerScore) -> String {
    let mut value = serde_json::to_value(record).expect("MinerScore serializes");
    value.sort_all_objects();
    format!(
        "miner_score {}",
        serde_json::to_string(&value).expect("score serializes")
    )
}

pub fn log_score_records(records: &[MinerScore]) {
    for record in records {
        tracing::info!("{}", score_line(record));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sn46_shared::{epoch_summary::EpochSummary, scoring::score_epoch_summary};

    #[test]
    fn golden_score_logs_preserve_order_and_large_integers() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/scoring_cases.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let mut summary: EpochSummary =
                serde_json::from_str(include_str!("../tests/fixtures/epoch_summary_v2.json"))
                    .unwrap();
            summary.summary_id = case["epoch_summary"]["summary_id"]
                .as_str()
                .unwrap()
                .to_owned();
            summary.miners =
                serde_json::from_value(case["epoch_summary"]["miners"].clone()).unwrap();
            let lines: Vec<_> = score_epoch_summary(&summary)
                .iter()
                .map(score_line)
                .collect();
            assert_eq!(
                serde_json::json!(lines),
                case["log_lines"],
                "{}",
                case["name"]
            );
        }
    }
}
