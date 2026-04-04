use serde_json::Value;

#[derive(Debug, Clone)]
pub enum PendingInteractionAction {
    Approve,
    ApproveForSession,
    Deny,
    Cancel,
    ReplyText(String),
    ReplyJson(Value),
}

#[derive(Debug, Clone)]
pub enum PendingInteractionKind {
    CommandApproval {
        command: Option<String>,
        cwd: Option<String>,
        reason: Option<String>,
    },
    FileChangeApproval,
    PermissionsApproval {
        permissions: Value,
        reason: Option<String>,
    },
    UserInputRequest {
        questions: Vec<Value>,
    },
}

#[derive(Debug, Clone)]
pub struct PendingInteractionSummary {
    pub token: String,
    pub kind: PendingInteractionKind,
    pub prompt: String,
}

impl PendingInteractionKind {
    pub fn summary(&self, token: String) -> PendingInteractionSummary {
        let prompt = match self {
            PendingInteractionKind::CommandApproval {
                command,
                cwd,
                reason,
            } => {
                let mut lines = vec![format!("Codex requests command approval (`{token}`).")];
                if let Some(command) = command.as_deref().filter(|text| !text.trim().is_empty()) {
                    lines.push(String::new());
                    lines.push("Command:".to_owned());
                    lines.push("```bash".to_owned());
                    lines.push(command.to_owned());
                    lines.push("```".to_owned());
                }
                if let Some(cwd) = cwd.as_deref().filter(|text| !text.trim().is_empty()) {
                    lines.push(format!("Working directory: `{cwd}`"));
                }
                if let Some(reason) = reason.as_deref().filter(|text| !text.trim().is_empty()) {
                    lines.push(String::new());
                    lines.push(reason.to_owned());
                }
                lines.push(String::new());
                lines.push("If this is the only pending request, reply with:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/approve".to_owned());
                lines.push("```".to_owned());
                lines.push("If there are multiple pending requests, use:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/approve 1".to_owned());
                lines.push("/approve-session 1".to_owned());
                lines.push("/deny 1".to_owned());
                lines.push("/cancel 1".to_owned());
                lines.push("```".to_owned());
                lines.push("You can still use the token directly if needed.".to_owned());
                lines.join("\n")
            }
            PendingInteractionKind::FileChangeApproval => [
                format!("Codex requests file-change approval (`{token}`)."),
                String::new(),
                "If this is the only pending request, reply with:".to_owned(),
                "```text".to_owned(),
                "/approve".to_owned(),
                "```".to_owned(),
                "If there are multiple pending requests, use:".to_owned(),
                "```text".to_owned(),
                "/approve 1".to_owned(),
                "/approve-session 1".to_owned(),
                "/deny 1".to_owned(),
                "/cancel 1".to_owned(),
                "```".to_owned(),
                "You can still use the token directly if needed.".to_owned(),
            ]
            .join("\n"),
            PendingInteractionKind::PermissionsApproval {
                permissions,
                reason,
            } => {
                let mut lines = vec![
                    format!("Codex requests additional permissions (`{token}`)."),
                    String::new(),
                    "Requested permissions:".to_owned(),
                    "```json".to_owned(),
                    permissions.to_string(),
                    "```".to_owned(),
                ];
                if let Some(reason) = reason.as_deref().filter(|text| !text.trim().is_empty()) {
                    lines.push(String::new());
                    lines.push(reason.to_owned());
                }
                lines.push(String::new());
                lines.push("If this is the only pending request, reply with:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/approve".to_owned());
                lines.push("```".to_owned());
                lines.push("If there are multiple pending requests, use:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/approve 1".to_owned());
                lines.push("/approve-session 1".to_owned());
                lines.push("/deny 1".to_owned());
                lines.push("/cancel 1".to_owned());
                lines.push("```".to_owned());
                lines.push("You can still use the token directly if needed.".to_owned());
                lines.join("\n")
            }
            PendingInteractionKind::UserInputRequest { questions } => {
                let mut lines = vec![format!("Codex requests user input (`{token}`).")];
                for question in questions {
                    let header = question
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or("question");
                    let body = question
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or("(missing question text)");
                    lines.push(format!("{header}: {body}"));

                    if let Some(options) = question.get("options").and_then(Value::as_array) {
                        let coded_options = render_coded_options(options);
                        if !coded_options.is_empty() {
                            lines.push("Options:".to_owned());
                            lines.push("```text".to_owned());
                            lines.extend(coded_options);
                            lines.push("```".to_owned());
                        }
                    }
                }
                if questions.len() == 1 {
                    lines.push("If this is the only pending request, reply with:".to_owned());
                    lines.push("```text".to_owned());
                    lines.push("/reply your answer".to_owned());
                    lines.push("```".to_owned());
                    lines.push("If there are multiple pending requests, use:".to_owned());
                    lines.push("```text".to_owned());
                    lines.push("/reply 1 your answer".to_owned());
                    lines.push("```".to_owned());
                } else {
                    lines.push("Reply one question at a time like this:".to_owned());
                    lines.push("```text".to_owned());
                    lines.push("/reply <question_id> your answer".to_owned());
                    lines.push("```".to_owned());
                    lines.push("If there are multiple pending requests, use:".to_owned());
                    lines.push("```text".to_owned());
                    lines.push("/reply 1 <question_id> your answer".to_owned());
                    lines.push("```".to_owned());
                }
                lines.push("You can still use the token directly if needed.".to_owned());
                lines.join("\n")
            }
        };

        PendingInteractionSummary {
            token,
            kind: self.clone(),
            prompt,
        }
    }
}

fn render_coded_options(options: &[Value]) -> Vec<String> {
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

fn coded_option_label(index: usize) -> String {
    format!("选项{}", option_code(index))
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

#[cfg(test)]
mod tests {
    use super::PendingInteractionKind;
    use serde_json::json;

    #[test]
    fn single_question_summary_lists_coded_options() {
        let summary = PendingInteractionKind::UserInputRequest {
            questions: vec![json!({
                "id": "color_choice",
                "header": "Color",
                "question": "Pick a color",
                "options": [
                    { "label": "Blue" },
                    { "label": "Green" }
                ]
            })],
        }
        .summary("req-1".to_owned());

        assert!(summary.prompt.contains("```text"));
        assert!(summary.prompt.contains("选项A"));
        assert!(summary.prompt.contains("选项B"));
    }

    #[test]
    fn command_approval_summary_includes_reply_examples() {
        let summary = PendingInteractionKind::CommandApproval {
            command: Some("echo hello".to_owned()),
            cwd: Some("/tmp".to_owned()),
            reason: Some("Need to confirm".to_owned()),
        }
        .summary("req-2".to_owned());

        assert!(summary.prompt.contains("```bash"));
        assert!(summary.prompt.contains("/approve"));
        assert!(summary.prompt.contains("Need to confirm"));
    }
}
