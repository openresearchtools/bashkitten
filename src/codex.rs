//! Native pinned openai-codex-responses request building and Responses conversion.
//! The seven built-in tools use JSON schemas; no deferred or grammar tools exist.
use anyhow::Result;
use serde_json::{Value, json};
fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}
fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

pub fn catalog() -> Vec<Value> {
    serde_json::from_str(include_str!("../reference/openai-codex-models.json"))
        .expect("pinned Codex catalog")
}
const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub fn thinking_levels(model: &Value) -> Vec<String> {
    if model["reasoning"] != true {
        return vec!["off".into()];
    }
    LEVELS
        .iter()
        .filter(|level| {
            let mapped = model["thinkingLevelMap"].get(**level);
            !mapped.is_some_and(Value::is_null)
                && (!matches!(**level, "xhigh" | "max") || mapped.is_some())
        })
        .map(|v| (*v).into())
        .collect()
}
pub fn clamp_thinking(model: &Value, level: &str) -> String {
    let available = thinking_levels(model);
    if available.iter().any(|v| v == level) {
        return level.into();
    }
    if let Some(index) = LEVELS.iter().position(|v| *v == level) {
        for candidate in LEVELS[index..].iter().chain(LEVELS[..index].iter().rev()) {
            if available.iter().any(|v| v == candidate) {
                return (*candidate).into();
            }
        }
    }
    available.first().cloned().unwrap_or_else(|| "off".into())
}
fn normalize_id_part(id: &str) -> String {
    id.encode_utf16()
        .take(64)
        .map(|c| {
            if c < 128 && ((c as u8).is_ascii_alphanumeric() || c == 95 || c == 45) {
                c as u8 as char
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_end_matches('_')
        .into()
}
fn normalize_tool_id(id: &str, model: &Value, source: &Value) -> String {
    if !matches!(
        s(&model["provider"]),
        "openai" | "openai-codex" | "opencode"
    ) || !id.contains('|')
    {
        return normalize_id_part(id);
    }
    let mut parts = id.split('|');
    let call = normalize_id_part(parts.next().unwrap_or_default());
    let item = parts.next().unwrap_or_default();
    let mut item = if source["provider"] != model["provider"] || source["api"] != model["api"] {
        format!("fc_{}", crate::providers::pi_short_hash(item))
    } else {
        normalize_id_part(item)
    };
    if !item.starts_with("fc_") {
        item = normalize_id_part(&format!("fc_{item}"));
    }
    format!("{call}|{item}")
}
fn text_item(text: &Value, signature: &Value, fallback: String) -> Value {
    let raw = s(signature);
    let parsed = if raw.starts_with('{') {
        serde_json::from_str::<Value>(raw).ok()
    } else {
        None
    };
    let parsed = parsed.filter(|v| v["v"] == 1 && v["id"].is_string());
    let mut id = parsed
        .as_ref()
        .map(|v| s(&v["id"]))
        .unwrap_or(raw)
        .to_owned();
    if id.is_empty() {
        id = fallback;
    } else if id.encode_utf16().count() > 64 {
        id = format!("msg_{}", crate::providers::pi_short_hash(&id));
    }
    let mut item = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}],"status":"completed","id":id});
    if let Some(phase) = parsed
        .as_ref()
        .and_then(|v| v["phase"].as_str())
        .filter(|v| matches!(*v, "commentary" | "final_answer"))
    {
        item["phase"] = json!(phase);
    }
    item
}
fn image_item(image: &Value) -> Value {
    json!({"type":"input_image","detail":"auto","image_url":format!("data:{};base64,{}",s(&image["mimeType"]),s(&image["data"]))})
}
pub fn tool_output(model: &Value, content: &[Value]) -> Value {
    let raw = crate::lossless_json::JsString::join(
        &content
            .iter()
            .filter(|v| v["type"] == "text")
            .map(|v| crate::lossless_json::JsString::from_value(&v["text"]).unwrap_or_default())
            .collect::<Vec<_>>(),
        "\n",
    );
    let has_text = !raw.is_empty();
    let text = raw.sanitized();
    let images: Vec<_> = content.iter().filter(|v| v["type"] == "image").collect();
    if images.is_empty() || !list(&model["input"]).iter().any(|v| v == "image") {
        return json!(if has_text {
            text
        } else if !images.is_empty() {
            "(see attached image)".into()
        } else {
            "(no tool output)".into()
        });
    }
    let mut output = Vec::new();
    if has_text {
        output.push(json!({"type":"input_text","text":text}));
    }
    output.extend(images.into_iter().map(image_item));
    json!(output)
}
pub fn convert_messages(model: &Value, context: &Value) -> Result<Vec<Value>> {
    let mut transformed = crate::completions::transform_messages_with(
        model,
        list(&context["messages"]),
        normalize_tool_id,
    );
    for message in &mut transformed {
        crate::completions::sanitize_message_text(message, false);
    }
    let mut output = Vec::new();
    let mut index = 0;
    for message in transformed {
        match s(&message["role"]) {
            "user" => {
                let content=if let Some(text)=message["content"].as_str(){vec![json!({"type":"input_text","text":text})]}else{
                    let content:Vec<_>=list(&message["content"]).iter().map(|v|if v["type"]=="text"{json!({"type":"input_text","text":v["text"]})}else{image_item(v)}).collect();
                    if content.is_empty(){continue;}content
                };
                output.push(json!({"role":"user","content":content}));
            },
            "assistant" => {
                let same_provider=message["provider"]==model["provider"] && message["api"]==model["api"];
                let same_model=same_provider && message["model"]==model["id"];
                let mut text_index=0;let mut items=Vec::new();
                for block in list(&message["content"]) {
                    match s(&block["type"]) {
                        "thinking" => if let Some(signature)=block["thinkingSignature"].as_str().filter(|s|!s.is_empty()){items.push(crate::lossless_json::from_str(signature)?);},
                        "text" => {
                            let fallback=if text_index==0{format!("msg_pi_{index}")}else{format!("msg_pi_{index}_{text_index}")};text_index+=1;
                            items.push(text_item(&block["text"],&block["textSignature"],fallback));
                        },
                        "toolCall" => {
                            let mut ids=s(&block["id"]).split('|');let call=ids.next().unwrap_or_default();let item=ids.next();
                            let mut value=json!({"type":"function_call"});
                            if let Some(id)=item.filter(|id|id.starts_with("fc_") && (!same_provider || same_model)){value["id"]=json!(id);}
                            value["call_id"]=json!(call);value["name"]=block["name"].clone();value["arguments"]=json!(crate::lossless_json::to_string(&block["arguments"])?);
                            if same_model && let Some(namespace)=block.get("namespace"){value["namespace"]=namespace.clone();}
                            items.push(value);
                        },
                        _=>{}
                    }
                }
                if items.is_empty(){continue;}output.extend(items);
            },
            "toolResult" => output.push(json!({"type":"function_call_output","call_id":s(&message["toolCallId"]).split('|').next().unwrap_or_default(),"output":tool_output(model,list(&message["content"]))})),
            _=>{}
        }
        index += 1;
    }
    Ok(output)
}
pub fn build_body(model: &Value, context: &Value, options: &Value) -> Result<Value> {
    let prompt =
        crate::lossless_json::JsString::from_value(&context["systemPrompt"]).unwrap_or_default();
    let verbosity = options["textVerbosity"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("low");
    let mut body = json!({"model":model["id"],"store":false,"stream":true,"instructions":if prompt.is_empty(){json!("You are a helpful assistant.")}else{prompt.to_value()},"input":convert_messages(model,context)?,"text":{"verbosity":verbosity},"include":["reasoning.encrypted_content"]});
    if options["cacheRetention"] != "none"
        && let Some(session) = options["sessionId"].as_str()
    {
        body["prompt_cache_key"] = json!(session.chars().take(64).collect::<String>());
    }
    body["tool_choice"] = options
        .get("toolChoice")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!("auto"));
    body["parallel_tool_calls"] = json!(true);
    for (option, key) in [
        ("temperature", "temperature"),
        ("serviceTier", "service_tier"),
    ] {
        if let Some(value) = options.get(option) {
            body[key] = value.clone();
        }
    }
    if !list(&context["tools"]).is_empty() {
        body["tools"]=json!(list(&context["tools"]).iter().map(|tool|{
        let mut value=json!({"type":"function","name":tool["name"],"description":tool["description"],"parameters":tool["parameters"]});
        if model["compat"]["supportsStrictMode"]!=false{value["strict"]=Value::Null;}value
    }).collect::<Vec<_>>());
    }
    if let Some(effort) = options["reasoningEffort"].as_str() {
        // Pi uses ?? here; an explicit null falls back to the named effort.
        let mapped = model["thinkingLevelMap"]
            .get(if effort == "none" { "off" } else { effort })
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!(effort));
        body["reasoning"] = json!({"effort":mapped,"summary":options.get("reasoningSummary").filter(|v|!v.is_null()).cloned().unwrap_or_else(||json!("auto"))});
    }
    Ok(body)
}
pub fn provider_body(request: &crate::providers::ProviderRequest) -> Result<Value> {
    let mut model=catalog().into_iter().find(|model|model["id"]==request.model).unwrap_or_else(||json!({"id":request.model,"api":"openai-codex-responses","provider":"openai-codex","reasoning":true}));
    model["input"] = if request.supports_images {
        json!(["text", "image"])
    } else {
        json!(["text"])
    };
    let messages = request
        .logical_messages
        .clone()
        .unwrap_or_else(|| crate::completions::fallback_messages(&request.messages, &model));
    let tools: Vec<_> = request
        .tools
        .iter()
        .map(|v| json!({"name":v.name,"description":v.description,"parameters":v.parameters}))
        .collect();
    let context = json!({"systemPrompt":request.system_prompt,"messages":messages,"tools":tools});
    let mut options = json!({"cacheRetention":if request.disable_cache{"none"}else{"short"}});
    let level = clamp_thinking(&model, request.thinking.as_str());
    if level != "off" {
        options["reasoningEffort"] = json!(level);
    }
    if let Some(session) = &request.session_id {
        options["sessionId"] = json!(session);
    }
    if let Some(temperature) = request.temperature {
        options["temperature"] = json!(temperature);
    }
    if let Some(choice) = &request.tool_choice {
        options["toolChoice"] = choice.clone();
    }
    for (key, value) in [
        ("serviceTier", &request.service_tier),
        ("textVerbosity", &request.text_verbosity),
        ("reasoningSummary", &request.reasoning_summary),
    ] {
        if let Some(value) = value {
            options[key] = json!(value);
        }
    }
    build_body(&model, &context, &options)
}
