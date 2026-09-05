use serde_json::{Value, json};
#[test]
fn pinned_codex_request_and_catalog_parity() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-codex-requests.json")).unwrap();
    assert_eq!(fixture["pin"], bashkitten::PI_REFERENCE_COMMIT);
    let models = bashkitten::models::codex_models();
    for entry in fixture["models"].as_array().unwrap() {
        let actual = models
            .iter()
            .find(|m| m.id == entry["model"]["id"])
            .unwrap();
        assert_eq!(actual.parameters, entry["model"]);
        assert_eq!(json!(actual.thinking_levels), entry["levels"]);
        let expected: bashkitten::agent::ModelCost =
            serde_json::from_value(entry["model"]["cost"].clone()).unwrap();
        assert_eq!(json!(actual.cost), json!(expected));
    }
    for case in fixture["cases"].as_array().unwrap() {
        let mut options = case["options"].clone();
        if case["simple"] == true {
            let level = bashkitten::codex::clamp_thinking(
                &case["model"],
                options["reasoning"].as_str().unwrap(),
            );
            if level != "off" {
                options["reasoningEffort"] = json!(level);
            }
        }
        let body =
            bashkitten::codex::build_body(&case["model"], &case["context"], &options).unwrap();
        assert_eq!(body, case["expected"], "{}", case["name"]);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&case["expected"]).unwrap(),
            "{} serialized order",
            case["name"]
        );
    }
}
