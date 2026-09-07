//! Actual pinned Pi UTF-16 fixtures, including the tool-to-history boundary.
use bashkitten::{
    agent, codex,
    lossless_json::{self, JsString},
    paths::AppPaths,
    session, tools,
};
use serde_json::{Value, json};
use std::fs;

#[tokio::test]
async fn pinned_pi_surrogates_survive_tool_history_jsonl_fork_and_live_wire() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-surrogate-wire.json")).unwrap();
    assert_eq!(fixture["pin"], bashkitten::PI_REFERENCE_COMMIT);
    for case in fixture["cases"].as_array().unwrap() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file.txt"), case["file"].as_str().unwrap()).unwrap();
        let result = tools::execute_tool(
            "grep",
            case["args"].clone(),
            &tools::ToolContext::new(temp.path()),
        )
        .await
        .unwrap();
        assert_eq!(
            lossless_json::to_string(&result).unwrap(),
            case["resultJson"].as_str().unwrap()
        );
        let tools::ContentBlock::Text { text } = &result.content[0] else {
            panic!("text expected")
        };
        if !case["textCodeUnits"].is_null() {
            assert_eq!(json!(text.units()), case["textCodeUnits"]);
        }
        assert_eq!(text.sanitized(), case["sanitizedText"].as_str().unwrap());

        let message: agent::AgentMessage =
            lossless_json::from_str(case["messageJson"].as_str().unwrap()).unwrap();
        assert_eq!(
            agent::estimate_tokens(&message),
            case["estimatedTokens"].as_u64().unwrap()
        );
        let mut logical = json!({"role":"toolResult","toolCallId":"call|fc_tool","toolName":"grep","content":result.content,"isError":false,"timestamp":2});
        let typed: agent::AgentMessage = serde_json::from_value(logical.clone()).unwrap();
        assert_eq!(typed, message);
        logical = serde_json::to_value(typed).unwrap();
        let paths = AppPaths {
            config: temp.path().join("config"),
            data: temp.path().join("data"),
            runtime: temp.path().join("runtime"),
        };
        paths.ensure().unwrap();
        let id = session::create(
            &paths,
            &session::NewSession {
                cwd: temp.path().to_owned(),
                model: "fixture/model".into(),
                thinking: "off".into(),
                model_parameters: json!({}),
                prompt: "Surrogate parity".into(),
                attachments: vec![],
                parent: None,
            },
        )
        .unwrap();
        let entry = json!({"type":"message","id":"surrogate-message","parentId":null,"timestamp":"2026-09-07T00:00:00Z","message":logical});
        session::append_values(&paths, &id, std::slice::from_ref(&entry)).unwrap();
        let loaded = session::read_segment(&paths, &id, 1)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(loaded, entry);
        let native: agent::SessionEntry = serde_json::from_value(loaded.clone()).unwrap();
        assert_eq!(native.context_messages(), vec![message]);
        let fork_id = session::fork_at(&paths, &id, "surrogate-message").unwrap();
        assert_eq!(
            session::read_segment(&paths, &fork_id, 1)
                .unwrap()
                .pop()
                .unwrap(),
            entry
        );

        let source_attachment = temp.path().join("retained.bin");
        fs::write(&source_attachment, b"retained attachment").unwrap();
        let copied = session::copy_attachments(&paths, &id, &[source_attachment]).unwrap();
        let mut reference = JsString::from_units(vec![0xd800]);
        reference.push_str(&format!("\nAttachment: {}", copied[0].display()));
        let attachment_entry = json!({"type":"message","id":"surrogate-reference","parentId":"surrogate-message","timestamp":"2026-09-07T00:00:01Z","message":{"role":"user","content":reference,"timestamp":3}});
        session::append_values(&paths, &id, &[attachment_entry]).unwrap();
        let fork = session::fork_at(&paths, &id, "surrogate-reference").unwrap();
        let relative = copied[0].strip_prefix(paths.session_dir(&id)).unwrap();
        assert_eq!(
            fs::read(paths.session_dir(&fork).join(relative)).unwrap(),
            b"retained attachment"
        );

        // The same serializable wrapper is used by HTTP history and socket/SSE
        // output: neither a replacement character nor an internal tag escapes.
        let live = serde_json::to_string(&lossless_json::wire(
            json!({"type":"tool_execution_end","result":result}),
        ))
        .unwrap();
        let received: Value = lossless_json::from_str(&live).unwrap();
        assert_eq!(
            lossless_json::to_string(&received["result"]).unwrap(),
            case["resultJson"].as_str().unwrap()
        );
        let history = json!({"messages":[case["assistant"],loaded["message"]]});
        assert_eq!(
            json!(codex::convert_messages(&case["model"], &history).unwrap()),
            case["providerItems"]
        );
    }
}

#[test]
fn pinned_pi_summary_slice_and_json_parse_stringify_preserve_code_units() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-surrogate-wire.json")).unwrap();
    for case in fixture["summaryCases"].as_array().unwrap() {
        let message = lossless_json::from_str(case["messageJson"].as_str().unwrap()).unwrap();
        let summary = agent::serialize_conversation(&[message]);
        assert_eq!(
            lossless_json::to_string(&summary).unwrap(),
            case["summaryJson"].as_str().unwrap()
        );
        assert_eq!(json!(summary.units()), case["summaryCodeUnits"]);
        assert_eq!(
            summary.sanitized(),
            case["sanitizedSummary"].as_str().unwrap()
        );
    }
    for case in fixture["serializationCases"].as_array().unwrap() {
        let input: JsString = lossless_json::from_str(case["inputJson"].as_str().unwrap()).unwrap();
        let value: Value = lossless_json::from_js_str(&input).unwrap();
        assert_eq!(
            lossless_json::to_string(&value).unwrap(),
            case["expectedJson"].as_str().unwrap()
        );
    }
    let first: JsString = lossless_json::from_str(r#""\ud83d""#).unwrap();
    let second: JsString = lossless_json::from_str(r#""\ude42""#).unwrap();
    assert_eq!(JsString::join(&[first, second], "").sanitized(), "🙂");
}

#[tokio::test]
async fn pinned_pi_tool_arguments_unicode_errors_and_disk_encoding() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pi-surrogate-wire.json")).unwrap();
    let mut failures = Vec::new();
    for case in fixture["toolCases"].as_array().unwrap() {
        let temp = tempfile::tempdir().unwrap();
        for path in case["dirs"].as_array().into_iter().flatten() {
            fs::create_dir_all(temp.path().join(path.as_str().unwrap())).unwrap();
        }
        for (path, content) in case["files"].as_object().unwrap() {
            fs::write(temp.path().join(path), content.as_str().unwrap()).unwrap();
        }
        let args = lossless_json::from_str(case["argsJson"].as_str().unwrap()).unwrap();
        let result = tools::execute_tool(
            case["tool"].as_str().unwrap(),
            args,
            &tools::ToolContext::new(temp.path()),
        )
        .await;
        let actual = match result {
            Ok(result) => json!({"result":result}),
            Err(error) => json!({"error":error.text()}),
        };
        let actual = lossless_json::to_string(&actual)
            .unwrap()
            .replace(temp.path().to_str().unwrap(), "<ROOT>");
        if actual != case["resultJson"].as_str().unwrap() {
            failures.push(format!(
                "{}: {actual}\nexpected {}",
                case["name"], case["resultJson"]
            ));
        }
        for (path, expected) in case["changed"].as_object().unwrap() {
            let actual = fs::read_to_string(temp.path().join(path)).unwrap_or_default();
            if actual != expected.as_str().unwrap() {
                failures.push(format!(
                    "{}: changed {path} = {actual:?}; expected {expected}",
                    case["name"]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
