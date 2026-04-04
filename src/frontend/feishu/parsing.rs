use anyhow::{Context, Result};
use serde_json::Value;

pub fn extract_inbound_text(message_type: Option<&str>, content: &str) -> Result<Option<String>> {
    match message_type {
        Some("text") | None => {
            let content: TextContent = serde_json::from_str(content)
                .context("failed to decode feishu text content json")?;
            Ok(content.text.filter(|text| !text.trim().is_empty()))
        }
        Some("post") => extract_post_text(content),
        Some(_) => extract_generic_text(content),
    }
}

#[derive(Debug, serde::Deserialize)]
struct TextContent {
    text: Option<String>,
}

fn extract_post_text(content: &str) -> Result<Option<String>> {
    let value: Value =
        serde_json::from_str(content).context("failed to decode feishu post content json")?;
    Ok(collect_text_fragments(&value)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty()))
}

fn extract_generic_text(content: &str) -> Result<Option<String>> {
    let value: Value =
        serde_json::from_str(content).context("failed to decode feishu message content json")?;
    Ok(collect_text_fragments(&value)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty()))
}

fn collect_text_fragments(value: &Value) -> Option<String> {
    let mut fragments = Vec::new();
    collect_text_fragments_into(value, &mut fragments);
    let joined = fragments
        .into_iter()
        .filter(|fragment| !fragment.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

fn collect_text_fragments_into(value: &Value, fragments: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                fragments.push(text.to_owned());
            }
            if let Some(title) = map.get("title").and_then(Value::as_str) {
                fragments.push(title.to_owned());
            }
            for child in map.values() {
                collect_text_fragments_into(child, fragments);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_text_fragments_into(item, fragments);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::extract_inbound_text;

    #[test]
    fn extracts_text_and_code_block_from_post_message() {
        let raw = r#"{"title":"","content":[[{"tag":"text","text":"intro","style":[]}],[{"tag":"code_block","language":"PLAIN_TEXT","text":"body\n"}]]}"#;
        let text = extract_inbound_text(Some("post"), raw).expect("text");
        assert_eq!(text.as_deref(), Some("intro\nbody"));
    }
}
