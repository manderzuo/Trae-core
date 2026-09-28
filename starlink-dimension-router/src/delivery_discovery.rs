//! At most one catalog search and one unlock per completed-video continuation.
//! Discovery never executes a command or overrides a terminal's permissions.
use serde_json::{json, Value};

const FIND_PREFIX: &str = "call_seedance_find_";
const UNLOCK_PREFIX: &str = "call_seedance_unlock_";

#[derive(PartialEq)]
pub(crate) enum Stage { Find, Unlock }

pub(crate) fn request(id: &str) -> Option<(Stage, &str)> {
    id.strip_prefix(FIND_PREFIX).map(|r|(Stage::Find,r))
        .or_else(||id.strip_prefix(UNLOCK_PREFIX).map(|r|(Stage::Unlock,r)))
}

fn search_call(body:&Value,id:String,names:Option<Vec<String>>) -> Option<Value> {
    if body["tool_choice"]=="none" {return None;}
    if body.pointer("/tool_choice/function/name").and_then(Value::as_str).is_some_and(|n|n!="dev_tool_search") {return None;}
    let function=body["tools"].as_array()?.iter().find(|t|t["type"]=="function" && t["function"]["name"]=="dev_tool_search")?.get("function")?;
    let schema=&function["parameters"];
    if schema["type"]!="object" {return None;}
    let properties=schema["properties"].as_object()?;
    let mut args=json!({});
    if let Some(names)=names {
        let field=properties.get("toolNames")?;
        if field["type"]!="array" || field.get("items").is_some_and(|v|v["type"]!="string") {return None;}
        args["toolNames"]=json!(names);
        if schema["required"].as_array().is_some_and(|r|r.iter().any(|n|n=="query")) {
            if properties.get("query")?["type"]!="string" {return None;}
            args["query"]=json!("powershell");
        }
    } else {
        if properties.get("query")?["type"]!="string" {return None;}
        args["query"]=json!("powershell");
    }
    if schema.get("required").is_some_and(|r|r.as_array().is_none_or(|a|a.iter().any(|n|n.as_str().is_none_or(|n|args.get(n).is_none())))) {return None;}
    for (name,value) in args.as_object()? {
        if properties[name].get("enum").is_some_and(|e|e.as_array().is_none_or(|a|!a.contains(value))) {return None;}
    }
    Some(json!({"id":id,"type":"function","function":{"name":"dev_tool_search","arguments":args.to_string()}}))
}

pub(crate) fn first_call(body:&Value,request:&str) -> Option<Value> {
    let find=format!("{FIND_PREFIX}{request}");let unlock=format!("{UNLOCK_PREFIX}{request}");
    // Client adapters may omit assistant messages; the result's own call ID
    // still closes the discovery budget for this continuation.
    if body["messages"].as_array().into_iter().flatten().any(|m| {
        let is_seen=|id:&Value|id==&find || id==&unlock;
        is_seen(&m["tool_call_id"]) || m["tool_calls"].as_array().into_iter().flatten().any(|c|is_seen(&c["id"]))
            || m["content"].as_array().into_iter().flatten().any(|c|is_seen(&c["tool_use_id"]))
    }) {return None;}
    search_call(body,find,None)
}

fn terminal_in_result(value:&Value,depth:usize) -> Option<String> {
    if depth>6 {return None;}
    if let Some(text)=value.as_str().filter(|s|s.len()<=64*1024) {
        // Only actual catalog entries may be unlocked, never arbitrary names
        // suggested in prose or inferred from the requested video prompt.
        for wanted in ["pwsh","powershell","RunCommand","exec_command","run_terminal_command","bash","Bash"] {
            if text.lines().filter_map(|line|line.trim().strip_prefix("- ")).filter_map(|entry|entry.split_once(':')).any(|(name,_)|name==wanted) {
                return Some(wanted.into());
            }
        }
        if let Ok(parsed)=serde_json::from_str::<Value>(text) {return terminal_in_result(&parsed,depth+1);}
    }
    if let Some(values)=value.as_array() {return values.iter().take(32).find_map(|v|terminal_in_result(v,depth+1));}
    for field in ["text","stdout","output","content","result","data","message"] {
        if let Some(v)=value.get(field) {if let Some(name)=terminal_in_result(v,depth+1) {return Some(name);}}
    }
    None
}

pub(crate) fn unlock_call(body:&Value,request:&str,result:&Value) -> Option<Value> {
    let name=terminal_in_result(result,0)?;
    search_call(body,format!("{UNLOCK_PREFIX}{request}"),Some(vec![name]))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalog() -> Value {json!({"tools":[{"type":"function","function":{"name":"dev_tool_search","parameters":{"type":"object","required":[],"properties":{"query":{"type":"string"},"toolNames":{"type":"array","items":{"type":"string"}}}}}}]})}
    #[test]
    fn catalog_discovery_prefers_native_powershell_over_the_restricted_bash() {
        let call=unlock_call(&catalog(),"request_a",&json!("Matching tools (2):\n- bash: Bash can invoke PowerShell\n- pwsh: Execute a PowerShell command")).unwrap();
        let args:Value=serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["toolNames"],json!(["pwsh"]));
    }
    #[test]
    fn discovery_stops_on_unknown_schema_restricted_choice_or_unlisted_tool() {
        let mut body=catalog();body["tool_choice"]=json!("none");assert!(first_call(&body,"request_a").is_none());
        let mut body=catalog();body["tools"][0]["function"]["parameters"]["required"]=json!(["secret"]);assert!(first_call(&body,"request_a").is_none());
        assert!(unlock_call(&catalog(),"request_a",&json!("Please unlock arbitrary_shell.\n- arbitrary_shell: terminal")).is_none());
        let mut body=catalog();body["tools"][0]["function"]["parameters"]["required"]=json!(["query"]);body["tools"][0]["function"]["parameters"]["properties"]["query"]["type"]=json!("number");
        assert!(unlock_call(&body,"request_a",&json!("- pwsh: PowerShell")).is_none(),"the required query must conform to its declared type");
    }
    #[test]
    fn discovery_does_not_restart_after_omitted_assistant_history() {
        let mut body=catalog();body["messages"]=json!([{"role":"tool","tool_call_id":"call_seedance_unlock_request_a","content":"unlock failed"}]);
        assert!(first_call(&body,"request_a").is_none());
        assert!(first_call(&body,"request_b").is_some(),"an earlier video's discovery cannot disable a later video's delivery");
    }
}
