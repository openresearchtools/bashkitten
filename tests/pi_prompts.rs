use bashkitten::{prompt, tools};
use serde_json::Value;
use std::{fs, path::Path};

#[test]
fn prompt_wording_tool_contracts_and_project_loading_match_pinned_pi() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/pi-prompts.json")).unwrap();
    assert_eq!(fixture["pin"], "9841914c71a74d81abe07f751aefd271fd924e63");
    let definitions = tools::tool_definitions();
    for (actual, expected) in definitions.iter().zip(fixture["tools"].as_array().unwrap()) {
        assert_eq!(actual.name, expected["name"], "tool name");
        assert_eq!(actual.label, expected["label"], "{} label", actual.name);
        assert_eq!(
            actual.description, expected["description"],
            "{} description",
            actual.name
        );
        assert_eq!(
            actual.parameters, expected["parameters"],
            "{} schema",
            actual.name
        );
        assert_eq!(
            actual.prompt_snippet, expected["promptSnippet"],
            "{} snippet",
            actual.name
        );
        assert_eq!(
            serde_json::to_value(&actual.prompt_guidelines).unwrap(),
            expected["promptGuidelines"],
            "{} guidelines",
            actual.name
        );
    }
    for case in fixture["cases"].as_array().unwrap() {
        let input = &case["input"];
        let contexts =
            serde_json::from_value::<Vec<prompt::ContextFile>>(input["contextFiles"].clone())
                .unwrap();
        let cwd = Path::new(input["cwd"].as_str().unwrap());
        let args = (
            input["customPrompt"].as_str(),
            input["appendSystemPrompt"].as_str(),
        );
        let actual = prompt::build(cwd, args.0, args.1, &contexts, &definitions, false);
        assert_eq!(actual, case["expected"], "{}", input["name"]);
        let passive = prompt::build(cwd, args.0, args.1, &contexts, &definitions, true);
        assert_eq!(
            passive.replace(&format!("\n\n{}", prompt::PASSIVE_SKILLS), ""),
            actual
        );
    }
    for case in fixture["contexts"].as_array().unwrap() {
        let temp = tempfile::tempdir().unwrap();
        for (path, content) in case["files"].as_object().unwrap() {
            let path = temp.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content.as_str().unwrap()).unwrap();
        }
        let cwd = temp.path().join(case["cwd"].as_str().unwrap());
        let config = temp.path().join(case["agentDir"].as_str().unwrap());
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&config).unwrap();
        let actual = prompt::load_project_context(&cwd, &config);
        let mut actual = serde_json::to_value(actual).unwrap();
        for file in actual.as_array_mut().unwrap() {
            file["path"] = Value::String(
                file["path"]
                    .as_str()
                    .unwrap()
                    .replace(temp.path().to_str().unwrap(), "<ROOT>"),
            );
        }
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}

#[test]
fn system_overrides_prefer_project_strip_bom_and_do_not_fall_through_empty_files() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    let config = temp.path().join("config");
    fs::create_dir_all(cwd.join(".pi")).unwrap();
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("SYSTEM.md"), "global").unwrap();
    assert_eq!(
        prompt::load_prompt_file(&cwd, &config, "SYSTEM.md").as_deref(),
        Some("global")
    );
    fs::write(cwd.join(".pi/SYSTEM.md"), "\u{feff}project\n").unwrap();
    assert_eq!(
        prompt::load_prompt_file(&cwd, &config, "SYSTEM.md").as_deref(),
        Some("project\n")
    );
    fs::write(cwd.join(".pi/SYSTEM.md"), "").unwrap();
    assert_eq!(
        prompt::load_prompt_file(&cwd, &config, "SYSTEM.md").as_deref(),
        Some("")
    );
    fs::write(config.join("APPEND_SYSTEM.md"), "append global").unwrap();
    fs::write(cwd.join(".pi/APPEND_SYSTEM.md"), "append project").unwrap();
    assert_eq!(
        prompt::load_prompt_file(&cwd, &config, "APPEND_SYSTEM.md").as_deref(),
        Some("append project")
    );
    fs::create_dir_all(config.join("skills")).unwrap();
    fs::write(
        config.join("skills/not-loaded.md"),
        "SKILL_CONTENT_MUST_NOT_APPEAR",
    )
    .unwrap();
    let paths = bashkitten::paths::AppPaths {
        config,
        data: temp.path().join("data"),
        runtime: temp.path().join("runtime"),
    };
    let actual = prompt::load(&paths, &cwd);
    assert!(actual.contains(prompt::PASSIVE_SKILLS));
    assert!(!actual.contains("SKILL_CONTENT_MUST_NOT_APPEAR"));
}
