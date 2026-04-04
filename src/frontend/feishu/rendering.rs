use serde_json::{json, Value};

pub fn looks_like_markdown_notice_post(text: &str) -> bool {
    text.contains("```")
}

pub fn build_notice_post(text: &str) -> Value {
    json!({
        "zh_cn": {
            "title": "",
            "content": build_notice_post_content_rows(text),
        }
    })
}

pub fn render_coded_options(options: &[Value]) -> Vec<String> {
    options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            option
                .get("label")
                .and_then(Value::as_str)
                .map(|label| format!("{}：{label}", coded_option_label(index)))
        })
        .collect()
}

pub fn coded_option_button_label(index: usize) -> String {
    if index == 0 {
        format!("{}（Recommand）", coded_option_label(index))
    } else {
        coded_option_label(index)
    }
}

pub fn coded_option_label(index: usize) -> String {
    format!("选项{}", option_code(index))
}

fn build_notice_post_content_rows(text: &str) -> Vec<Value> {
    parse_notice_blocks(text)
        .into_iter()
        .filter_map(|block| match block {
            NoticeBlock::Text(text) => {
                let text = text.trim();
                if text.is_empty() {
                    None
                } else {
                    Some(json!([
                        {
                            "tag": "text",
                            "text": text,
                            "style": [],
                        }
                    ]))
                }
            }
            NoticeBlock::Code(code) => {
                let code = code.trim_end_matches('\n');
                if code.trim().is_empty() {
                    None
                } else {
                    Some(json!([
                        {
                            "tag": "code_block",
                            "language": "PLAIN_TEXT",
                            "text": format!("{code}\n"),
                        }
                    ]))
                }
            }
        })
        .collect()
}

fn parse_notice_blocks(text: &str) -> Vec<NoticeBlock> {
    let mut blocks = Vec::new();
    let mut buffer = String::new();
    let mut in_code_block = false;

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code_block {
                if !buffer.is_empty() {
                    blocks.push(NoticeBlock::Code(std::mem::take(&mut buffer)));
                }
                in_code_block = false;
            } else {
                if !buffer.trim().is_empty() {
                    blocks.extend(
                        buffer
                            .split("\n\n")
                            .map(str::trim)
                            .filter(|segment| !segment.is_empty())
                            .map(|segment| NoticeBlock::Text(segment.to_owned())),
                    );
                }
                buffer.clear();
                in_code_block = true;
            }
            continue;
        }

        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(line);
    }

    if in_code_block {
        if !buffer.is_empty() {
            blocks.push(NoticeBlock::Code(buffer));
        }
    } else if !buffer.trim().is_empty() {
        blocks.extend(
            buffer
                .split("\n\n")
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .map(|segment| NoticeBlock::Text(segment.to_owned())),
        );
    }

    blocks
}

fn option_code(mut index: usize) -> String {
    let mut chars = Vec::new();
    loop {
        let remainder = index % 26;
        chars.push((b'A' + remainder as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    chars.iter().rev().collect()
}

enum NoticeBlock {
    Text(String),
    Code(String),
}

#[cfg(test)]
mod tests {
    use super::{build_notice_post, coded_option_button_label, render_coded_options};
    use serde_json::json;

    #[test]
    fn notice_post_uses_text_and_code_block_sections() {
        let post = build_notice_post("Send this:\n\n```text\n/reply your answer\n```");
        let content = &post["zh_cn"]["content"];
        assert_eq!(content[0][0]["tag"], "text");
        assert_eq!(content[1][0]["tag"], "code_block");
    }

    #[test]
    fn coded_options_use_short_labels() {
        let rendered = render_coded_options(&[json!({"label":"Blue"}), json!({"label":"Green"})]);
        assert_eq!(rendered, vec!["选项A：Blue", "选项B：Green"]);
        assert_eq!(coded_option_button_label(0), "选项A（Recommand）");
    }
}
