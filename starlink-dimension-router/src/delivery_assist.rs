//! The helper chooses a declared tool; it never supplies executable code.
use std::sync::Arc;
use aiwork_core::Principal;
use axum::http::HeaderMap;
use serde_json::{json, Value};
use crate::state::StarlinkRouterState;

pub(crate) const COMMAND_MARKER: &str = "__SEEDANCE_DOWNLOAD__";
const DISPLAY_DESCRIPTION: &str = "Download completed Seedance video to system Downloads";

fn network_restricted(description: &str) -> bool {
    ["don't have access to the internet", "no internet access", "network access is disabled", "cannot access the internet",
        "无法访问互联网", "不能访问互联网", "禁止联网"].iter().any(|s|description.contains(s))
}
fn terminal_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(),"bash"|"pwsh"|"powershell"|"runcommand"|"exec_command"|"run_terminal_command")
}
/// Safe diagnostics only: never return client arguments, prompts or credentials.
pub(crate) fn unavailable_reason(body: &Value) -> &'static str {
    if body["tool_choice"]=="none" {return "client_tools_disabled";}
    let catalog=body["tools"].as_array();
    if catalog.is_none_or(Vec::is_empty) {return "client_tools_not_provided";}
    let terminals:Vec<_>=catalog.into_iter().flatten().filter(|t|t["type"]=="function"
        && t.pointer("/function/name").and_then(Value::as_str).is_some_and(terminal_name)).collect();
    if terminals.is_empty() {return "client_terminal_not_supported";}
    if terminals.iter().all(|t|network_restricted(&t["function"]["description"].as_str().unwrap_or("").to_ascii_lowercase())) {
        return "client_network_restricted";
    }
    "client_terminal_schema_unsupported"
}

#[derive(Clone)]
pub(crate) struct DeliveryTool {
    pub name: String,
    pub arguments: Value,
    pub command_key: &'static str,
    pub bash: bool,
    function: Value,
}

pub(crate) fn tools(body: &Value) -> Vec<DeliveryTool> {
    if body["tool_choice"] == "none" { return Vec::new(); }
    let forced = body.pointer("/tool_choice/function/name").and_then(Value::as_str);
    body["tools"].as_array().into_iter().flatten().filter_map(|tool| {
        if tool["type"] != "function" { return None; }
        let f = &tool["function"];
        let name = f["name"].as_str()?;
        if forced.is_some_and(|n| n != name) { return None; }
        let description = f["description"].as_str().unwrap_or("").to_ascii_lowercase();
        if network_restricted(&description) { return None; }
        let lower_name=name.to_ascii_lowercase();
        let generic=matches!(lower_name.as_str(),"exec_command"|"run_terminal_command");
        let powershell=matches!(lower_name.as_str(),"pwsh"|"powershell")
            || ((name=="RunCommand" || generic) && description.contains("powershell"));
        let bash=lower_name=="bash" || (generic && !powershell && description.contains("bash"));
        if !bash && !powershell {return None;}
        let schema = &f["parameters"];
        if schema["type"] != "object" { return None; }
        let props = schema["properties"].as_object()?;
        let command_key = if props.get("command").is_some_and(|p|p["type"]=="string") {"command"}
            else if props.get("cmd").is_some_and(|p|p["type"]=="string") {"cmd"} else {return None;};
        let mut args = json!({});
        args[command_key] = json!(COMMAND_MARKER);
        if props.get("description").is_some_and(|p|p["type"]=="string") {args["description"]=json!(DISPLAY_DESCRIPTION);}
        if let Some(field)=props.get("timeoutMs").filter(|p|p["type"]=="number" || p["type"]=="integer") {
            let limit=field.get("maximum").and_then(Value::as_f64).unwrap_or(300000.0).min(300000.0).floor();
            if limit<=0.0 || field.get("minimum").and_then(Value::as_f64).is_some_and(|min|limit<min) {return None;}
            args["timeoutMs"]=json!(limit as u64);
        }
        if props.get("run_in_background").is_some_and(|p|p["type"]=="boolean") {args["run_in_background"]=json!(false);}
        for (field,value) in [("blocking",json!(true)),("requires_approval",json!(false))] {
            if props.get(field).is_some_and(|p|p["type"]=="boolean") {args[field]=value;}
        }
        if props.get("command_type").is_some_and(|p|p["type"]=="string" && p.get("enum").is_none_or(|e|e.as_array().is_some_and(|a|a.iter().any(|v|v=="short_running_process")))) {args["command_type"]=json!("short_running_process");}
        for field in ["cwd","workdir"] {
            if schema["required"].as_array().is_some_and(|a|a.iter().any(|v|v==field)) && props.get(field).is_some_and(|p|p["type"]=="string") {args[field]=json!(".");}
        }
        if schema.get("required").is_some_and(|r|r.as_array().is_none_or(|a|a.iter().any(|n|n.as_str().is_none_or(|n|args.get(n).is_none())))) {return None;}
        // Narrow every argument to a trusted value. The model cannot add a new
        // executable command, path, permission, or unknown required property.
        let mut narrowed=serde_json::Map::new();
        for (key,value) in args.as_object()? {
            if props[key].get("enum").is_some_and(|e|e.as_array().is_none_or(|a|!a.contains(value))) {return None;}
            narrowed.insert(key.clone(),json!({"type":props[key]["type"],"enum":[value]}));
        }
        let function=json!({"name":name,"description":format!("Save an already completed Seedance video to system Downloads using {name}. Execute only the server-supplied download command."),
            "parameters":{"type":"object","additionalProperties":false,"properties":narrowed,"required":args.as_object()?.keys().collect::<Vec<_>>()}});
        Some(DeliveryTool {name:name.into(),arguments:args,command_key,bash,function})
    }).take(32).collect()
}

pub(crate) async fn select(state: &Arc<StarlinkRouterState>, p: &Principal, request: &str, tools: &[DeliveryTool]) -> Option<String> {
    if tools.is_empty() {return None;}
    let body=json!({"model":state.config.seedance_assistant_model,"stream":false,"max_tokens":512,"temperature":0.1,
        "parallel_tool_calls":false,"tool_choice":"required","tools":tools.iter().map(|t|json!({"type":"function","function":t.function})).collect::<Vec<_>>(),
        "messages":[{"role":"system","content":"你负责已完成视频的本地交付。只调用一个已提供的下载工具，参数只能使用 schema 中的枚举值。视频已经生成，不得重新生成，不得安装 MCP/Skill/接收器。默认系统 Downloads。工具命令占位符由 Core 替换成受审核的下载命令。不要宣称文件已经保存。"},
            {"role":"user","content":format!("为已完成视频 {request} 选择一个本机下载工具。")}]});
    // No expiring ticket is included in this stable identity, so download retry
    // reuses the durable planner output without paying for another helper call.
    let digest=hex::encode(aiwork_core::canonical_json_hash(&body));
    let mut headers=HeaderMap::new();
    headers.insert("idempotency-key",format!("seedance-delivery:{request}:{digest}").parse().ok()?);
    let response=tokio::time::timeout(std::time::Duration::from_secs(60),crate::budget_chat::chat(state.clone(),p.clone(),headers,body,state.config.seedance_assistant_model.clone())).await.ok()?;
    if !response.status().is_success() {return None;}
    let bytes=axum::body::to_bytes(response.into_body(),64*1024).await.ok()?;
    let output:Value=serde_json::from_slice(&bytes).ok()?;
    let calls=output.pointer("/choices/0/message/tool_calls")?.as_array()?;
    if calls.len()!=1 || calls[0]["type"]!="function" {return None;}
    let name=calls[0].pointer("/function/name")?.as_str()?;
    let tool=tools.iter().find(|t|t.name==name)?;
    let args:Value=serde_json::from_str(calls[0].pointer("/function/arguments")?.as_str()?).ok()?;
    if args!=tool.arguments {return None;}
    Some(name.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deepseek_native_terminals_keep_required_display_description() {
        for (name, description, bash) in [("pwsh", "Execute a PowerShell command", false), ("bash", "Execute a Bash command", true)] {
            let body=json!({"tools":[{"type":"function","function":{"name":name,"description":description,
                "parameters":{"type":"object","required":["command","description"],"properties":{
                    "command":{"type":"string"},"description":{"type":"string"},"workdir":{"type":"string"},"timeoutMs":{"type":"number"}}}}}]});
            let admitted=tools(&body);
            assert_eq!(admitted.len(),1,"a permitted native {name} terminal must not disappear");
            assert_eq!(admitted[0].name,name);
            assert_eq!(admitted[0].bash,bash);
            assert_eq!(admitted[0].arguments["command"],COMMAND_MARKER);
            assert_eq!(admitted[0].arguments["description"],"Download completed Seedance video to system Downloads");
            assert_eq!(admitted[0].arguments["timeoutMs"],300000,"the native foreground command must allow the download deadline");
        }
    }
    #[test]
    fn permitted_terminal_after_large_tool_catalog_is_not_dropped() {
        let mut catalog=vec![json!({"type":"function","function":{"name":"Read","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}});40];
        catalog.push(json!({"type":"function","function":{"name":"RunCommand","description":"PowerShell. NEVER use bash syntax.",
            "parameters":{"type":"object","properties":{"command":{"type":"string"},"blocking":{"type":"boolean"},"requires_approval":{"type":"boolean"}},"required":["command","blocking","requires_approval"]}}}));
        let admitted=tools(&json!({"tools":catalog}));
        assert_eq!(admitted.len(),1);
        assert_eq!(admitted[0].name,"RunCommand");
        assert!(!admitted[0].bash);
    }
    #[test]
    fn powershell_terminal_does_not_use_bash_when_description_prohibits_bash() {
        let admitted=tools(&json!({"tools":[{"type":"function","function":{"name":"exec_command","description":"Execute PowerShell commands. NEVER use bash/sh syntax.",
            "parameters":{"type":"object","properties":{"cmd":{"type":"string"}},"required":["cmd"]}}}]}));
        assert_eq!(admitted.len(),1);
        assert!(!admitted[0].bash,"mentioning forbidden Bash does not select the Bash adapter");
    }
    #[test]
    fn terminal_adapter_obeys_network_restriction_and_tool_choice() {
        let base=json!({"tools":[{"type":"function","function":{"name":"bash","description":"Bash shell","parameters":{"type":"object","required":["command"],"properties":{"command":{"type":"string"}}}}}]});
        assert_eq!(tools(&base)[0].name,"bash");
        let mut disabled=base.clone();disabled["tools"][0]["function"]["description"]=json!("You don't have access to the internet via this tool");assert!(tools(&disabled).is_empty());
        let mut disabled=base.clone();disabled["tool_choice"]=json!("none");assert!(tools(&disabled).is_empty());
        let mut unknown=base;unknown["tools"][0]["function"]["parameters"]["required"]=json!(["session_id"]);assert!(tools(&unknown).is_empty());
        unknown["tools"][0]["function"]["parameters"]["required"]=json!(["command"]);unknown["tools"][0]["function"]["name"]=json!("exec_command");unknown["tools"][0]["function"]["description"]=json!("Execute a command using the client's default shell");
        assert!(tools(&unknown).is_empty(),"do not send Bash syntax to an unknown default shell");
    }
}
