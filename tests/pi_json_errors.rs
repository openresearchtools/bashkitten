use bashkitten::{agent::ModelCost, codex_stream, response::ResponseAssembly};
use serde_json::Value;

fn normalize(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, normalize(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(normalize).collect()),
        Value::Number(value) => serde_json::json!(value.as_f64().unwrap()),
        value => value,
    }
}
#[test]
fn malformed_json_matches_pinned_pi_syntax_and_stream_errors() {
    let fixture: Value =
        bashkitten::lossless_json::from_str(include_str!("fixtures/pi-json-errors.json")).unwrap();
    let cost: ModelCost = serde_json::from_value(fixture["model"]["cost"].clone()).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(
            bashkitten::json_error::error_text(input),
            bashkitten::lossless_json::JsString::from_value(&case["expected"]).unwrap(),
            "input {input:?}"
        );
        let mut decoder = codex_stream::Decoder::default();
        let mut state = codex_stream::State::new("gpt-5.5".into());
        let mut output = ResponseAssembly::default();
        let mut failed = false;
        for frame in decoder.feed(case["payload"].as_str().unwrap().as_bytes(), true) {
            match frame.and_then(|frame| state.push(&frame)) {
                Ok(events) => {
                    for event in events {
                        output.push(event);
                    }
                }
                Err(error) => {
                    output.fail(bashkitten::json_error::exception_message(&error));
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            output.fail("OpenAI Responses stream ended before a terminal response event");
        }
        let mut actual =
            serde_json::to_value(output.message("openai-codex", "gpt-5.5", &cost)).unwrap();
        actual.as_object_mut().unwrap().remove("timestamp");
        assert_eq!(
            normalize(actual),
            normalize(case["output"].clone()),
            "stream input {input:?}"
        );
    }
}
