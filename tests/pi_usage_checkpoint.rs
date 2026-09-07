use bashkitten::{agent, usage};
use serde_json::Value;

#[test]
fn numbered_checkpoint_preserves_pi_footer_and_cost_addition_order() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-usage-checkpoint.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let omitted: Vec<agent::SessionEntry> =
            serde_json::from_value(case["omitted"].clone()).unwrap();
        let retained: Vec<agent::SessionEntry> =
            serde_json::from_value(case["retained"].clone()).unwrap();
        let before = agent::session_usage_totals(&omitted);
        let snapshot = usage::snapshot_with_cache(
            &retained,
            &agent::build_session_context(&retained, None),
            before,
            usage::latest_cache_hit_rate(&omitted).flatten(),
            2000,
            false,
            true,
        );
        assert_eq!(
            snapshot.text,
            case["text"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        let totals: agent::UsageTotals = serde_json::from_value(case["totals"].clone()).unwrap();
        assert_eq!(snapshot.totals, totals, "{}", case["name"]);
    }
}
