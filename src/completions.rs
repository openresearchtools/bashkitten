//! Pinned Pi openai-completions.ts request construction and transform-messages.ts.
//! Operates on complete logical messages so provider/model identity and signatures
//! survive switches and worker restarts. No provider or catalog network calls.
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

fn s(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

pub fn compatibility(model: &Value) -> Value {
    let provider = s(&model["provider"]);
    let url = s(&model["baseUrl"]);
    let id = s(&model["id"]);
    let zai = matches!(provider, "zai" | "zai-coding-cn")
        || url.contains("api.z.ai")
        || url.contains("open.bigmodel.cn");
    let together = provider == "together"
        || url.contains("api.together.ai")
        || url.contains("api.together.xyz");
    let moonshot =
        matches!(provider, "moonshotai" | "moonshotai-cn") || url.contains("api.moonshot.");
    let openrouter = provider == "openrouter" || url.contains("openrouter.ai");
    let cloudflare = provider == "cloudflare-workers-ai" || url.contains("api.cloudflare.com");
    let gateway = provider == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
    let nvidia = provider == "nvidia" || url.contains("integrate.api.nvidia.com");
    let ant = provider == "ant-ling" || url.contains("api.ant-ling.com");
    let deepseek = provider == "deepseek" || url.to_lowercase().contains("deepseek.com");
    let grok = provider == "xai" || url.contains("api.x.ai");
    let nonstandard = nvidia
        || provider == "cerebras"
        || url.contains("cerebras.ai")
        || grok
        || together
        || url.contains("chutes.ai")
        || deepseek
        || zai
        || moonshot
        || provider == "opencode"
        || url.contains("opencode.ai")
        || cloudflare
        || gateway
        || ant;
    let max_tokens = url.contains("chutes.ai")
        || deepseek
        || moonshot
        || gateway
        || together
        || nvidia
        || ant
        || zai;
    let mut out = json!({
        "supportsStore":!nonstandard,
        "supportsDeveloperRole":(openrouter&&(id.starts_with("anthropic/")||id.starts_with("openai/")))||(!nonstandard&&!openrouter),
        "supportsReasoningEffort":!grok&&!zai&&!moonshot&&!together&&!gateway&&!nvidia&&!ant,
        "supportsUsageInStreaming":true,"supportsFinishReason":true,
        "maxTokensField":if max_tokens{"max_tokens"}else{"max_completion_tokens"},
        "requiresToolResultName":false,"requiresAssistantAfterToolResult":false,"requiresThinkingAsText":false,
        "requiresReasoningContentOnAssistantMessages":deepseek,
        "thinkingFormat":if deepseek{"deepseek"}else if zai{"zai"}else if together{"together"}else if ant{"ant-ling"}else if openrouter{"openrouter"}else{"openai"},
        "openRouterRouting":{},"vercelGatewayRouting":{},"chatTemplateKwargs":{},"chatTemplateArgs":{},
        "zaiToolStream":false,"supportsThinkingTokenBudget":false,"supportsStrictMode":!moonshot&&!together&&!gateway&&!nvidia,
        "supportsOpenAIGrammarTools":false,"sendSessionAffinityHeaders":false,
        "sessionAffinityFormat":if openrouter{"openrouter"}else{"openai"},
        "supportsLongCacheRetention":!together&&!cloudflare&&!gateway&&!nvidia&&!ant
    });
    if provider == "openrouter" && id.starts_with("anthropic/") {
        out["cacheControlFormat"] = json!("anthropic");
    }
    for (key, value) in model["compat"].as_object().into_iter().flatten() {
        if !value.is_null() {
            out[key] = value.clone();
        }
    }
    out
}

pub fn normalize_tool_id(id: &str, provider: &str) -> String {
    if let Some((call, item)) = id.split_once('|') {
        let sanitize = |v: &str| {
            v.encode_utf16()
                .map(|c| {
                    if c <= 127 && ((c as u8).is_ascii_alphanumeric() || c == 95 || c == 45) {
                        char::from_u32(c as u32).unwrap()
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        let call = sanitize(call);
        let item = sanitize(item);
        let combined = if item.is_empty() {
            call.clone()
        } else {
            format!("{call}_{item}")
        };
        if combined.len() <= 40 {
            return combined;
        }
        let hash = crate::providers::pi_short_hash(id)
            .chars()
            .take(8)
            .collect::<String>();
        return format!(
            "{}_{hash}",
            call.chars().take(40 - hash.len() - 1).collect::<String>()
        );
    }
    if provider == "openai" {
        id.chars().take(40).collect()
    } else {
        id.into()
    }
}

pub fn transform_messages(model: &Value, messages: &[Value]) -> Vec<Value> {
    transform_messages_with(model, messages, |id, model, _| {
        normalize_tool_id(id, s(&model["provider"]))
    })
}

pub(crate) fn transform_messages_with(
    model: &Value,
    messages: &[Value],
    normalize: impl Fn(&str, &Value, &Value) -> String,
) -> Vec<Value> {
    let vision = list(&model["input"]).iter().any(|v| v == "image");
    let mut ids = HashMap::<String, String>::new();
    let mut transformed = Vec::new();
    for original in messages {
        let mut message = original.clone();
        if message["content"].is_null() {
            message["content"] = json!([]);
        }
        if !vision
            && matches!(s(&message["role"]), "user" | "toolResult")
            && message["content"].is_array()
        {
            let placeholder = if message["role"] == "user" {
                "(image omitted: model does not support images)"
            } else {
                "(tool image omitted: model does not support images)"
            };
            let mut content = Vec::new();
            let mut previous = false;
            for block in list(&message["content"]) {
                if block["type"] == "image" {
                    if !previous {
                        content.push(json!({"type":"text","text":placeholder}));
                    }
                    previous = true;
                } else {
                    content.push(block.clone());
                    previous = block["text"] == placeholder;
                }
            }
            message["content"] = json!(content);
        }
        if message["role"] == "toolResult" {
            if let Some(id) = ids.get(s(&message["toolCallId"])) {
                message["toolCallId"] = json!(id);
            }
        } else if message["role"] == "assistant" {
            let same = message["provider"] == model["provider"]
                && message["api"] == model["api"]
                && message["model"] == model["id"];
            let mut content = Vec::new();
            for block in list(&message["content"]) {
                match s(&block["type"]) {
                    "thinking" => {
                        if truthy(&block["redacted"]) {
                            if same {
                                content.push(block.clone());
                            }
                            continue;
                        }
                        if same && truthy(&block["thinkingSignature"]) {
                            content.push(block.clone());
                            continue;
                        }
                        if s(&block["thinking"]).trim().is_empty() {
                            continue;
                        }
                        content.push(if same {
                            block.clone()
                        } else {
                            json!({"type":"text","text":block["thinking"]})
                        });
                    }
                    "text" => content.push(if same {
                        block.clone()
                    } else {
                        json!({"type":"text","text":block["text"]})
                    }),
                    "toolCall" => {
                        let mut call = block.clone();
                        if !same {
                            call.as_object_mut().unwrap().remove("thoughtSignature");
                            let id = normalize(s(&call["id"]), model, &message);
                            if id != s(&call["id"]) {
                                ids.insert(s(&call["id"]).into(), id.clone());
                                call["id"] = json!(id);
                            }
                        }
                        content.push(call);
                    }
                    _ => content.push(block.clone()),
                }
            }
            message["content"] = json!(content);
        }
        transformed.push(message);
    }
    let mut result = Vec::new();
    let mut pending = Vec::new();
    let mut existing = HashSet::new();
    fn insert(result: &mut Vec<Value>, pending: &mut Vec<Value>, existing: &mut HashSet<String>) {
        for call in pending.drain(..) {
            if !existing.contains(s(&call["id"])) {
                result.push(json!({"role":"toolResult","toolCallId":call["id"],"toolName":call["name"],"content":[{"type":"text","text":"No result provided"}],"isError":true}));
            }
        }
        existing.clear();
    }
    for message in transformed {
        if message["role"] == "assistant" {
            insert(&mut result, &mut pending, &mut existing);
            if matches!(s(&message["stopReason"]), "error" | "aborted") {
                continue;
            }
            pending = list(&message["content"])
                .iter()
                .filter(|v| v["type"] == "toolCall")
                .cloned()
                .collect();
        } else if message["role"] == "toolResult" {
            existing.insert(s(&message["toolCallId"]).into());
        } else if message["role"] == "user" {
            insert(&mut result, &mut pending, &mut existing);
        }
        result.push(message);
    }
    insert(&mut result, &mut pending, &mut existing);
    result
}

pub(crate) fn valid_reasoning_detail(v: &Value) -> bool {
    if !v.is_object()
        || v.get("id").is_some_and(|v| !v.is_null() && !v.is_string())
        || v.get("format").is_some_and(|v| !v.is_string())
        || v.get("index").is_some_and(|v| !v.is_number())
    {
        return false;
    }
    match s(&v["type"]) {
        "reasoning.summary" => v["summary"].is_string(),
        "reasoning.encrypted" => v["data"].is_string(),
        "reasoning.text" => {
            v["text"].is_string()
                && v.get("signature")
                    .is_none_or(|v| v.is_null() || v.is_string())
        }
        _ => false,
    }
}
fn reasoning_details(blocks: &[&Value], calls: &[&Value]) -> Option<Value> {
    for block in blocks {
        if let Ok(details) = serde_json::from_str::<Value>(s(&block["thinkingSignature"]))
            && details.is_array()
            && !list(&details).is_empty()
            && list(&details).iter().all(valid_reasoning_detail)
        {
            return Some(details);
        }
    }
    let details: Vec<_> = calls
        .iter()
        .filter_map(|call| serde_json::from_str::<Value>(s(&call["thoughtSignature"])).ok())
        .filter(|v| {
            valid_reasoning_detail(v)
                && v["type"] == "reasoning.encrypted"
                && !s(&v["id"]).is_empty()
                && !s(&v["data"]).is_empty()
        })
        .collect();
    if details.is_empty() {
        None
    } else {
        Some(json!(details))
    }
}
fn image_part(block: &Value) -> Value {
    json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",s(&block["mimeType"]),s(&block["data"]))}})
}
fn bridge() -> Value {
    json!({"role":"assistant","content":"I have processed the tool results."})
}

pub fn convert_messages(model: &Value, context: &Value, compat: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut last = "";
    if !s(&context["systemPrompt"]).is_empty() {
        out.push(json!({"role":if model["reasoning"]==true&&compat["supportsDeveloperRole"]==true{"developer"}else{"system"},"content":context["systemPrompt"]}));
    }
    let transformed = transform_messages(model, list(&context["messages"]));
    let mut i = 0;
    while i < transformed.len() {
        let message = &transformed[i];
        let role = s(&message["role"]);
        let blocks = list(&message["content"]);
        if compat["requiresAssistantAfterToolResult"] == true
            && last == "toolResult"
            && role == "user"
        {
            out.push(bridge());
        }
        if role == "user" {
            if message["content"].is_string() {
                out.push(json!({"role":"user","content":message["content"]}));
            } else if !blocks.is_empty() {
                out.push(json!({"role":"user","content":blocks.iter().map(|b|if b["type"]=="text"{json!({"type":"text","text":b["text"]})}else{image_part(b)}).collect::<Vec<_>>()}));
            }
        } else if role == "assistant" {
            let texts: Vec<_> = blocks
                .iter()
                .filter(|b| b["type"] == "text" && !s(&b["text"]).trim().is_empty())
                .map(|b| json!({"type":"text","text":b["text"]}))
                .collect();
            let text = texts.iter().map(|b| s(&b["text"])).collect::<String>();
            let thinking: Vec<_> = blocks.iter().filter(|b| b["type"] == "thinking").collect();
            let calls: Vec<_> = blocks.iter().filter(|b| b["type"] == "toolCall").collect();
            let details = reasoning_details(&thinking, &calls);
            let nonempty: Vec<_> = thinking
                .iter()
                .filter(|b| !s(&b["thinking"]).trim().is_empty())
                .collect();
            let mut assistant = json!({"role":"assistant","content":if compat["requiresAssistantAfterToolResult"]==true{json!("")}else{Value::Null}});
            if !nonempty.is_empty() && compat["requiresThinkingAsText"] == true {
                let thinking_text = nonempty
                    .iter()
                    .map(|b| s(&b["thinking"]))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let mut content = vec![json!({"type":"text","text":thinking_text})];
                content.extend(texts);
                assistant["content"] = json!(content);
            } else {
                if !text.is_empty() {
                    assistant["content"] = json!(text);
                }
                if !nonempty.is_empty() && details.is_none() {
                    let signature = s(&nonempty[0]["thinkingSignature"]);
                    let signature =
                        if model["provider"] == "opencode-go" && signature == "reasoning" {
                            "reasoning_content"
                        } else {
                            signature
                        };
                    if matches!(
                        signature,
                        "reasoning_content" | "reasoning" | "reasoning_text"
                    ) {
                        assistant[signature] = json!(
                            nonempty
                                .iter()
                                .map(|b| s(&b["thinking"]))
                                .collect::<Vec<_>>()
                                .join("\n")
                        );
                    }
                }
            }
            if !calls.is_empty() {
                assistant["tool_calls"]=json!(calls.iter().map(|call|json!({"id":call["id"],"type":"function","function":{"name":call["name"],"arguments":call["arguments"].to_string()}})).collect::<Vec<_>>());
            }
            if let Some(details) = details {
                assistant["reasoning_details"] = details;
            }
            if compat["requiresReasoningContentOnAssistantMessages"] == true
                && model["reasoning"] == true
                && assistant.get("reasoning_content").is_none()
            {
                assistant["reasoning_content"] = json!("");
            }
            if (!truthy(&assistant["content"])
                || assistant["content"].as_array().is_some_and(Vec::is_empty))
                && calls.is_empty()
            {
                i += 1;
                continue;
            }
            out.push(assistant);
        } else if role == "toolResult" {
            let mut images = Vec::new();
            let mut added = HashSet::new();
            while i < transformed.len() && transformed[i]["role"] == "toolResult" {
                let message = &transformed[i];
                let blocks = list(&message["content"]);
                let text = blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .map(|b| s(&b["text"]))
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = blocks.iter().any(|b| b["type"] == "image");
                let mut result = json!({"role":"tool","content":if !text.is_empty(){text}else if has_images{"(see attached image)".into()}else{"(no tool output)".into()},"tool_call_id":message["toolCallId"]});
                if compat["requiresToolResultName"] == true && truthy(&message["toolName"]) {
                    result["name"] = message["toolName"].clone();
                }
                out.push(result);
                if list(&model["input"]).iter().any(|v| v == "image") {
                    images.extend(
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "image")
                            .map(image_part),
                    );
                }
                if compat["deferredToolsMode"] == "kimi" {
                    for name in list(&message["addedToolNames"]) {
                        added.insert(s(name).to_owned());
                    }
                }
                i += 1;
            }
            if !images.is_empty() {
                if compat["requiresAssistantAfterToolResult"] == true {
                    out.push(bridge());
                }
                let mut content =
                    vec![json!({"type":"text","text":"Attached image(s) from tool result:"})];
                content.extend(images);
                out.push(json!({"role":"user","content":content}));
                last = "user";
            } else {
                last = "toolResult";
            }
            if !added.is_empty() {
                let tools: Vec<_> = list(&context["tools"])
                    .iter()
                    .filter(|t| added.contains(s(&t["name"])))
                    .map(|t| convert_tool(t, compat))
                    .collect();
                if !tools.is_empty() {
                    out.push(json!({"role":"system","tools":tools}));
                }
            }
            continue;
        }
        last = role;
        i += 1;
    }
    out
}

fn convert_tool(tool: &Value, compat: &Value) -> Value {
    let mut function = json!({"name":tool["name"],"description":tool["description"],"parameters":tool["parameters"]});
    if compat["supportsStrictMode"] != false {
        function["strict"] = tool
            .pointer("/constrainedSampling/strict")
            .cloned()
            .unwrap_or(json!(false));
    }
    json!({"type":"function","function":function})
}

fn mapped_effort<'a>(model: &'a Value, effort: &str) -> Option<&'a Value> {
    model["thinkingLevelMap"].get(effort)
}
fn budget(model: &Value, options: &Value, max_tokens: Option<f64>) -> Option<f64> {
    let effort = s(&options["reasoningEffort"]);
    if effort.is_empty() || model["reasoning"] != true {
        return None;
    }
    let level = if matches!(effort, "xhigh" | "max") {
        "high"
    } else {
        effort
    };
    let default = match level {
        "minimal" => 1024.0,
        "low" => 2048.0,
        "medium" => 8192.0,
        _ => 16384.0,
    };
    let value = options["thinkingBudgets"][level]
        .as_f64()
        .unwrap_or(default);
    let ceiling = max_tokens
        .or_else(|| model["maxTokens"].as_f64())
        .unwrap_or(0.0);
    let value = value.min((ceiling - 1024.0).max(0.0));
    (value > 0.0).then_some(value)
}
fn template_values(
    model: &Value,
    options: &Value,
    values: &Value,
    budget: Option<f64>,
) -> Option<Value> {
    let mut output = Map::new();
    let effort = s(&options["reasoningEffort"]);
    let enabled = !effort.is_empty();
    for (key, value) in values.as_object().into_iter().flatten() {
        let resolved = if !value.is_object() && !value.is_array() {
            Some(value.clone())
        } else if !enabled && truthy(&value["omitWhenOff"]) {
            None
        } else {
            match s(&value["$var"]) {
                "thinking.enabled" => Some(json!(enabled)),
                "thinking.budget" => budget.map(|v| json!(v)),
                _ => match mapped_effort(model, if enabled { effort } else { "off" }) {
                    Some(Value::String(v)) => Some(json!(v)),
                    None if enabled => Some(json!(effort)),
                    _ => None,
                },
            }
        };
        if let Some(value) = resolved {
            output.insert(key.clone(), value);
        }
    }
    if output.is_empty() {
        None
    } else {
        Some(Value::Object(output))
    }
}
fn apply_thinking(body: &mut Value, model: &Value, options: &Value, compat: &Value) {
    let effort = s(&options["reasoningEffort"]);
    let enabled = !effort.is_empty();
    let reasoning = model["reasoning"] == true;
    let supports = compat["supportsReasoningEffort"] == true;
    let mapped = mapped_effort(model, if enabled { effort } else { "off" });
    let fallback = || {
        mapped
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!(effort))
    };
    let thinking_budget = budget(
        model,
        options,
        body["max_tokens"]
            .as_f64()
            .or_else(|| body["max_completion_tokens"].as_f64()),
    );
    match s(&compat["thinkingFormat"]) {
        "zai" if reasoning => {
            body["thinking"] = if enabled {
                json!({"type":"enabled","clear_thinking":false})
            } else {
                json!({"type":"disabled"})
            };
            if enabled
                && supports
                && let Some(value) = mapped
                    .or(Some(&options["reasoningEffort"]))
                    .filter(|v| v.is_string())
            {
                body["reasoning_effort"] = value.clone();
            }
        }
        "qwen" if reasoning => {
            body["enable_thinking"] = json!(enabled);
            if enabled && supports && fallback().is_string() {
                body["reasoning_effort"] = fallback();
            }
        }
        "qwen-chat-template" if reasoning => {
            body["chat_template_kwargs"] =
                json!({"enable_thinking":enabled,"preserve_thinking":true});
        }
        "chat-template" if reasoning => {
            if let Some(values) = template_values(
                model,
                options,
                &compat["chatTemplateKwargs"],
                thinking_budget,
            ) {
                body["chat_template_kwargs"] = values;
            }
        }
        "baseten" if reasoning => {
            if let Some(values) =
                template_values(model, options, &compat["chatTemplateArgs"], thinking_budget)
            {
                body["chat_template_args"] = values;
            }
            if supports
                && let Some(value) = mapped
                    .or_else(|| enabled.then_some(&options["reasoningEffort"]))
                    .filter(|v| v.is_string())
            {
                body["reasoning_effort"] = value.clone();
            }
        }
        "deepseek" if reasoning => {
            if enabled {
                body["thinking"] = json!({"type":"enabled"});
            } else if mapped != Some(&Value::Null) {
                body["thinking"] = json!({"type":"disabled"});
            }
            if enabled && supports {
                body["reasoning_effort"] = fallback();
            }
        }
        "openrouter" if reasoning => {
            if enabled {
                body["reasoning"] = json!({"effort":fallback()});
            } else if mapped != Some(&Value::Null) {
                body["reasoning"] = json!({"effort":mapped.cloned().unwrap_or(json!("none"))});
            }
        }
        "ant-ling" if reasoning && enabled => {
            if let Some(value) = mapped.filter(|v| v.is_string()) {
                body["reasoning"] = json!({"effort":value});
            }
        }
        "together" if reasoning => {
            body["reasoning"] = json!({"enabled":enabled});
            if enabled && supports {
                body["reasoning_effort"] = fallback();
            }
        }
        "string-thinking" if reasoning => {
            if enabled {
                body["thinking"] = fallback();
            } else if mapped != Some(&Value::Null) {
                body["thinking"] = mapped.cloned().unwrap_or(json!("none"));
            }
        }
        _ if enabled && reasoning && supports => {
            body["reasoning_effort"] = fallback();
        }
        _ if !enabled && reasoning && supports => {
            if let Some(value) = mapped.filter(|v| v.is_string()) {
                body["reasoning_effort"] = value.clone();
            }
        }
        _ => {}
    }
    let field = compat["thinkingTokenBudgetField"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            (compat["supportsThinkingTokenBudget"] == true).then_some("thinking_token_budget")
        });
    if let (Some(field), Some(budget)) = (field, thinking_budget) {
        body[field] = json!(budget);
    }
}

fn cache_message(message: &mut Value, control: &Value) -> bool {
    if let Some(text) = message["content"].as_str() {
        if text.is_empty() {
            return false;
        }
        message["content"] = json!([{"type":"text","text":text,"cache_control":control}]);
        return true;
    }
    if let Some(parts) = message["content"].as_array_mut() {
        for part in parts.iter_mut().rev() {
            if part["type"] == "text" {
                part["cache_control"] = control.clone();
                return true;
            }
        }
    }
    false
}

pub fn build_params(model: &Value, context: &Value, options: &Value) -> Value {
    let compat = compatibility(model);
    let mut messages = convert_messages(model, context, &compat);
    let retention = options["cacheRetention"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("short");
    let cache_long = retention == "long" && compat["supportsLongCacheRetention"] == true;
    let cache_control = if compat["cacheControlFormat"] == "anthropic" && retention != "none" {
        Some(if cache_long {
            json!({"type":"ephemeral","ttl":"1h"})
        } else {
            json!({"type":"ephemeral"})
        })
    } else {
        None
    };
    let mut body = json!({"model":model["id"],"messages":[],"stream":true});
    if ((s(&model["baseUrl"]).contains("api.openai.com") && retention != "none") || cache_long)
        && options["sessionId"].is_string()
    {
        body["prompt_cache_key"] = json!(
            s(&options["sessionId"])
                .chars()
                .take(64)
                .collect::<String>()
        );
    }
    if cache_long {
        body["prompt_cache_retention"] = json!("24h");
    }
    if compat["supportsUsageInStreaming"] != false {
        body["stream_options"] = json!({"include_usage":true});
    }
    if compat["supportsStore"] == true {
        body["store"] = json!(false);
    }
    if truthy(&options["maxTokens"]) {
        body[s(&compat["maxTokensField"])] = options["maxTokens"].clone();
    }
    if let Some(value) = options.get("temperature") {
        body["temperature"] = value.clone();
    }
    let deferred: HashSet<_> = if compat["deferredToolsMode"] == "kimi" {
        list(&context["messages"])
            .iter()
            .filter(|m| m["role"] == "toolResult")
            .flat_map(|m| list(&m["addedToolNames"]))
            .map(s)
            .collect()
    } else {
        HashSet::new()
    };
    let tools: Vec<_> = list(&context["tools"])
        .iter()
        .filter(|t| !deferred.contains(s(&t["name"])))
        .map(|t| convert_tool(t, &compat))
        .collect();
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        if compat["zaiToolStream"] == true {
            body["tool_stream"] = json!(true);
        }
    } else if list(&context["messages"]).iter().any(|m| {
        m["role"] == "toolResult"
            || (m["role"] == "assistant"
                && list(&m["content"]).iter().any(|b| b["type"] == "toolCall"))
    }) {
        body["tools"] = json!([]);
    }
    if let Some(control) = cache_control {
        if let Some(system) = messages
            .iter_mut()
            .find(|m| matches!(s(&m["role"]), "system" | "developer"))
        {
            cache_message(system, &control);
        }
        if let Some(tool) = body["tools"].as_array_mut().and_then(|v| v.last_mut()) {
            tool["cache_control"] = control.clone();
        }
        for message in messages.iter_mut().rev() {
            if matches!(s(&message["role"]), "user" | "assistant" | "tool")
                && cache_message(message, &control)
            {
                break;
            }
        }
    }
    body["messages"] = json!(messages);
    if truthy(&options["toolChoice"]) {
        body["tool_choice"] = options["toolChoice"].clone();
    }
    if let Some(priority) = compat.get("vllmPriority") {
        body["priority"] = priority.clone();
    }
    apply_thinking(&mut body, model, options, &compat);
    if let Some(routing) = model["compat"]
        .get("openRouterRouting")
        .filter(|v| truthy(v))
    {
        body["provider"] = routing.clone();
    }
    if let Some(routing) = model["compat"].get("vercelGatewayRouting") {
        let mut gateway = Map::new();
        for key in ["only", "order"] {
            if let Some(value) = routing.get(key).filter(|v| truthy(v)) {
                gateway.insert(key.into(), value.clone());
            }
        }
        if !gateway.is_empty() {
            body["providerOptions"] = json!({"gateway":gateway});
        }
    }
    for (key, value) in options["samplingParams"].as_object().into_iter().flatten() {
        body[key] = value.clone();
    }
    body
}

fn text_tokens(value: &str) -> u64 {
    (value.encode_utf16().count() as u64).div_ceil(4)
}
fn message_tokens(message: &Value) -> u64 {
    if let Some(text) = message["content"].as_str() {
        return text_tokens(text);
    }
    let mut chars = 0;
    for block in list(&message["content"]) {
        chars += match s(&block["type"]) {
            "text" => s(&block["text"]).encode_utf16().count(),
            "thinking" => s(&block["thinking"]).encode_utf16().count(),
            "image" => 4800,
            _ => {
                s(&block["name"]).encode_utf16().count()
                    + block["arguments"].to_string().encode_utf16().count()
            }
        };
    }
    (chars as u64).div_ceil(4)
}
pub fn estimate_context(context: &Value) -> u64 {
    let messages = list(&context["messages"]);
    let mut latest = f64::NEG_INFINITY;
    let mut usage = None;
    for (index, message) in messages.iter().enumerate() {
        let timestamp = message["timestamp"].as_f64().unwrap_or(f64::NAN);
        let value = &message["usage"];
        let total = value["totalTokens"]
            .as_u64()
            .filter(|n| *n != 0)
            .unwrap_or_else(|| {
                ["input", "output", "cacheRead", "cacheWrite"]
                    .iter()
                    .filter_map(|k| value[*k].as_u64())
                    .sum()
            });
        if message["role"] == "assistant"
            && timestamp >= latest
            && !matches!(s(&message["stopReason"]), "error" | "aborted")
            && total > 0
        {
            usage = Some((index, total));
        }
        latest = latest.max(timestamp);
    }
    if let Some((index, total)) = usage {
        let added: HashSet<_> = messages[index + 1..]
            .iter()
            .filter(|m| m["role"] == "toolResult")
            .flat_map(|m| list(&m["addedToolNames"]))
            .map(s)
            .collect();
        let tools: Vec<_> = list(&context["tools"])
            .iter()
            .filter(|t| added.contains(s(&t["name"])))
            .cloned()
            .collect();
        total
            + messages[index + 1..]
                .iter()
                .map(message_tokens)
                .sum::<u64>()
            + if tools.is_empty() {
                0
            } else {
                text_tokens(&json!(tools).to_string())
            }
    } else {
        messages.iter().map(message_tokens).sum::<u64>()
            + text_tokens(s(&context["systemPrompt"]))
            + if list(&context["tools"]).is_empty() {
                0
            } else {
                text_tokens(&context["tools"].to_string())
            }
    }
}
pub fn simple_options(model: &Value, context: &Value, options: &Value) -> Value {
    let mut output = options.clone();
    let max = options["maxTokens"]
        .as_f64()
        .or_else(|| model["maxTokens"].as_f64())
        .unwrap_or(1.0);
    let context_window = model["contextWindow"].as_f64().unwrap_or(0.0);
    output["maxTokens"] = json!(if context_window <= 0.0 {
        max.max(1.0)
    } else {
        max.min((context_window - estimate_context(context) as f64 - 4096.0).max(1.0))
    });
    if model["samplingParams"].is_object() || options["samplingParams"].is_object() {
        let mut params = model["samplingParams"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        params.extend(
            options["samplingParams"]
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        output["samplingParams"] = json!(params);
    }
    output
}

pub fn preset_model(
    provider: &str,
    base_url: &str,
    preset: &crate::config::ModelPreset,
    llama: bool,
) -> Value {
    let mut compat = if llama {
        json!({"supportsStore":false,"supportsDeveloperRole":false,"supportsReasoningEffort":false,"supportsUsageInStreaming":true,"supportsStrictMode":false,"maxTokensField":"max_tokens"})
    } else {
        json!({})
    };
    // Older BashKitten presets declared these two fields separately. Explicit
    // values retain their meaning; absent values now use Pi's detection defaults.
    if let Some(value) = preset.supports_developer_role {
        compat["supportsDeveloperRole"] = json!(value);
    }
    if let Some(value) = preset.supports_reasoning_effort {
        compat["supportsReasoningEffort"] = json!(value);
    }
    for (key, value) in preset.compatibility.as_object().into_iter().flatten() {
        compat[key] = value.clone();
    }
    json!({"id":preset.id,"name":preset.name,"provider":provider,"api":"openai-completions","baseUrl":base_url,"contextWindow":preset.context_window,"maxTokens":preset.max_tokens,"input":preset.input,"reasoning":preset.reasoning,"compat":compat,"thinkingLevelMap":preset.thinking_level_map,"samplingParams":preset.request_parameters,"thinkingBudgets":preset.thinking_budgets})
}

pub fn fallback_messages(
    messages: &[crate::providers::ProviderMessage],
    model: &Value,
) -> Vec<Value> {
    use crate::providers::{ContentPart, MessageRole};
    let mut output = Vec::new();
    for message in messages {
        let mut content = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text {
                    text,
                    text_signature,
                } => {
                    let mut value = json!({"type":"text","text":text});
                    if let Some(signature) = text_signature {
                        value["textSignature"] = json!(signature);
                    }
                    content.push(value);
                }
                ContentPart::Thinking {
                    text,
                    id,
                    encrypted_content,
                } => {
                    let mut value = json!({"type":"thinking","thinking":text});
                    if let Some(signature) = encrypted_content {
                        value["thinkingSignature"] = json!(signature);
                    } else if let Some(signature) = id {
                        value["thinkingSignature"] = json!(signature);
                    }
                    content.push(value);
                }
                ContentPart::Image { source } => match source {
                    crate::providers::ImageSource::Base64 {
                        media_type, data, ..
                    } => content.push(json!({"type":"image","mimeType":media_type,"data":data})),
                    crate::providers::ImageSource::Url { url, .. } => {
                        if let Some((mime, data)) = url
                            .strip_prefix("data:")
                            .and_then(|url| url.split_once(";base64,"))
                        {
                            content.push(json!({"type":"image","mimeType":mime,"data":data}));
                        }
                    }
                },
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => content
                    .push(json!({"type":"toolCall","id":id,"name":name,"arguments":arguments})),
                ContentPart::ToolResult {
                    tool_call_id,
                    output: result,
                    is_error,
                } => {
                    let blocks = if let Some(text) = result.as_str() {
                        vec![json!({"type":"text","text":text})]
                    } else {
                        list(result).iter().filter_map(|v|{
                        if v["type"]=="input_text"{Some(json!({"type":"text","text":v["text"]}))}
                        else{s(&v["image_url"]).strip_prefix("data:").and_then(|url|url.split_once(";base64,")).map(|(mime,data)|json!({"type":"image","mimeType":mime,"data":data}))}
                    }).collect()
                    };
                    output.push(json!({"role":"toolResult","toolCallId":tool_call_id,"content":blocks,"isError":is_error,"timestamp":0}));
                }
            }
        }
        if message.role != MessageRole::Tool {
            output.push(if message.role==MessageRole::Assistant{json!({"role":"assistant","content":content,"api":model["api"],"provider":model["provider"],"model":model["id"],"stopReason":"stop","timestamp":0})}else{json!({"role":"user","content":content,"timestamp":0})});
        }
    }
    output
}

pub fn provider_body(request: &crate::providers::ProviderRequest, model: &Value) -> Value {
    let mut model = model.clone();
    model["id"] = json!(request.model);
    model["input"] = if request.supports_images {
        json!(["text", "image"])
    } else {
        json!(["text"])
    };
    let tools:Vec<_>=request.tools.iter().map(|tool|json!({"name":tool.name,"description":tool.description,"parameters":tool.parameters})).collect();
    let context = json!({"systemPrompt":request.system_prompt,"messages":request.logical_messages.clone().unwrap_or_else(||fallback_messages(&request.messages,&model)),"tools":tools});
    let mut options = json!({"samplingParams":request.request_parameters,"cacheRetention":if request.disable_cache{"none"}else if std::env::var("PI_CACHE_RETENTION").as_deref()==Ok("long"){"long"}else{"short"},"thinkingBudgets":model["thinkingBudgets"]});
    if request.thinking != crate::providers::ThinkingLevel::Off {
        options["reasoningEffort"] = json!(request.thinking.as_str());
    }
    if let Some(value) = request.max_tokens {
        options["maxTokens"] = json!(value);
    }
    if let Some(value) = request.temperature {
        options["temperature"] = json!(value);
    }
    if let Some(value) = &request.tool_choice {
        options["toolChoice"] = value.clone();
    }
    if let Some(value) = &request.session_id {
        options["sessionId"] = json!(value);
    }
    let options = simple_options(&model, &context, &options);
    build_params(&model, &context, &options)
}
