use bashkitten::completions;
use serde_json::Value;
fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(k, a)| b.get(k).is_some_and(|b| equal(a, b)))
        }
        _ => a == b,
    }
}
#[test]
fn pinned_compatible_detection_requests_reasoning_history_cache_and_limits() {
    let fixture: Value =
        bashkitten::lossless_json::from_str(include_str!("fixtures/pi-completions.json")).unwrap();
    assert_eq!(fixture["pin"], bashkitten::PI_REFERENCE_COMMIT);
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let compat = completions::compatibility(&case["model"]);
        if !equal(&compat, &case["compat"]) {
            failures.push(format!(
                "{} compatibility: {compat} expected {}",
                case["name"], case["compat"]
            ));
        }
        let options = if case["simple"] == true {
            completions::simple_options(&case["model"], &case["context"], &case["options"])
        } else {
            case["options"].clone()
        };
        let actual = completions::build_params(&case["model"], &case["context"], &options);
        if !equal(&actual, &case["expected"]) {
            failures.push(format!(
                "{} request: {actual} expected {}",
                case["name"], case["expected"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
