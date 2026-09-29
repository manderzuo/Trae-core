//! Accept presentation fences, never repair or search for a hidden decision.
use serde_json::Value;

pub(crate) fn object(raw: &str, max_bytes: usize) -> Result<Value, ()> {
    if raw.len() > max_bytes {
        return Err(());
    }
    let trimmed = raw.trim();
    let json = if trimmed.starts_with("```") {
        let (opening, rest) = trimmed.split_once('\n').ok_or(())?;
        match opening.trim() {
            "```" | "```json" | "```JSON" => {}
            _ => return Err(()),
        }
        let (contents, closing) = rest.rsplit_once('\n').ok_or(())?;
        if closing.trim() != "```" {
            return Err(());
        }
        contents.trim()
    } else {
        trimmed
    };
    let value: Value = serde_json::from_str(json).map_err(|_| ())?;
    if !value.is_object() {
        return Err(());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn prompt_backticks_inside_json_are_data_not_a_second_wrapper() {
        let raw="```json\n{\"prompt\":\"保留文字 ```json 和符号\",\"intent\":\"video\"}\n```";
        assert_eq!(super::object(raw,16384).unwrap()["prompt"],"保留文字 ```json 和符号");
    }
    #[test]
    fn object_limit_includes_whitespace_and_fences() {
        assert!(super::object("```json\n{}\n```",13).is_err());
        assert!(super::object("   {}",4).is_err());
        assert!(super::object("[]",16).is_err());
    }
}
