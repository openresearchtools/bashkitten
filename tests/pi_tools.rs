use base64::Engine;
use bashkitten::{
    tool_validation,
    tools::{self, ToolContext},
};
use serde_json::{Value, json};
use std::os::unix::ffi::OsStringExt;
use std::{fs, os::unix::fs::symlink};

#[tokio::test]
async fn pinned_pi_tools_arguments_results_failures_and_filesystem_effects() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pi-tools.json")).unwrap();
    assert_eq!(fixture["pin"], bashkitten::PI_REFERENCE_COMMIT);
    // Pi returns the installed fd's stderr verbatim. Debian Bookworm ships
    // fd 8.6 (clap 3); the main oracle uses fd 10.5 (clap 4). Both expected
    // variants are actual pinned Pi executions, with no error normalization.
    let fd8: Value = serde_json::from_str(include_str!("fixtures/pi-tools-fd8.json")).unwrap();
    assert_eq!(fd8["pin"], bashkitten::PI_REFERENCE_COMMIT);
    let fd_version = ["fd", "fdfind"]
        .iter()
        .find_map(|program| {
            std::process::Command::new(program)
                .arg("--version")
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .unwrap();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let root = tempfile::tempdir().unwrap();
        for (name, content) in case["files"].as_object().unwrap() {
            let path = root.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content.as_str().unwrap()).unwrap();
        }
        for (name, content) in case["binaryFiles"].as_object().into_iter().flatten() {
            fs::write(
                root.path().join(name),
                base64::engine::general_purpose::STANDARD
                    .decode(content.as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
        }
        for file in case["rawFiles"].as_array().into_iter().flatten() {
            let filename = std::ffi::OsString::from_vec(
                base64::engine::general_purpose::STANDARD
                    .decode(file["nameBase64"].as_str().unwrap())
                    .unwrap(),
            );
            fs::write(
                root.path().join(filename),
                base64::engine::general_purpose::STANDARD
                    .decode(file["contentBase64"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
        }
        for dir in case["dirs"].as_array().into_iter().flatten() {
            fs::create_dir_all(root.path().join(dir.as_str().unwrap())).unwrap();
        }
        for (name, target) in case["links"].as_object().into_iter().flatten() {
            symlink(target.as_str().unwrap(), root.path().join(name)).unwrap();
        }
        let name = case["name"].as_str().unwrap();
        let tool = case["tool"].as_str().unwrap();
        let arguments = case["rawArgs"]
            .as_str()
            .map(|raw| serde_json::from_str(raw).unwrap())
            .unwrap_or_else(|| case["args"].clone());
        let mut context = ToolContext::new(root.path());
        let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::<tools::ToolResult>::new()));
        context.model_supports_images = case["nonVision"] != true;
        if case["aborted"] == true {
            context.cancellation.cancel();
        }
        let result = if case["validationOnly"] == true {
            tool_validation::prepare(tool, arguments.clone())
                .map(|value| json!({"validated":value}))
        } else {
            let cancellation = context.cancellation.clone();
            let abort_text = case["abortWhenOutput"].as_str().map(str::to_owned);
            let update_results = updates.clone();
            let capture_updates = case["captureUpdates"] == true;
            let update = move |result: tools::ToolResult| {
                if capture_updates {
                    update_results.lock().unwrap().push(result.clone());
                }
                if abort_text.as_deref().is_some_and(|text| {
                    result
                        .text_content()
                        .is_some_and(|value| value.contains(text))
                }) {
                    cancellation.cancel();
                }
            };
            tools::execute_tool_with_updates(tool, arguments, &context, Some(&update))
                .await
                .map(|value| json!({"result":value}))
        };
        let mut actual = result.unwrap_or_else(|error| json!({"error":error.to_string()}));
        if case["captureUpdates"] == true {
            actual["updates"] = json!(*updates.lock().unwrap());
        }
        let actual: Value = serde_json::from_str(
            &actual
                .to_string()
                .replace(root.path().to_str().unwrap(), "<ROOT>"),
        )
        .unwrap();
        // JSON numbers are JS numbers; 1 and 1.0 are observably the same.
        fn equivalent(a: &Value, b: &Value) -> bool {
            match (a, b) {
                (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
                (Value::Array(a), Value::Array(b)) => {
                    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equivalent(a, b))
                }
                (Value::Object(a), Value::Object(b)) => {
                    a.len() == b.len()
                        && a.iter()
                            .all(|(key, a)| b.get(key).is_some_and(|b| equivalent(a, b)))
                }
                _ => a == b,
            }
        }
        let expected = if fd_version == fd8["fdVersion"].as_str().unwrap() {
            fd8["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|other| other["name"] == name)
                .map(|other| &other["expected"])
                .unwrap_or(&case["expected"])
        } else {
            &case["expected"]
        };
        if !equivalent(&actual, expected) {
            failures.push(format!("{name}: expected {}\nactual {actual}", expected));
        }
        for (name, content) in case["changed"].as_object().unwrap() {
            let actual = fs::read_to_string(root.path().join(name))
                .ok()
                .map(Value::String)
                .unwrap_or(Value::Null);
            if actual != *content {
                failures.push(format!(
                    "{} changed {name}: expected {content}, got {actual}",
                    case["name"]
                ));
            }
        }
        for (name, exists) in case["directories"].as_object().into_iter().flatten() {
            if root.path().join(name).exists() != exists.as_bool().unwrap() {
                failures.push(format!("{tool} directory {name}: expected exists={exists}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
