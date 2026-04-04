use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

use crate::codex_app_server::AppServerClient;
use crate::config::CodexConfig;
pub use crate::domain::interaction::PendingInteractionAction;
use crate::model::{CollaborationModePreset, PendingInteractionSummary};

#[derive(Debug, Clone)]
pub struct CodexRequest {
    pub session_id: Option<String>,
    pub prompt: String,
    pub working_directory: PathBuf,
    pub collaboration_mode: Option<CollaborationModePreset>,
}

#[derive(Debug, Clone)]
pub struct CodexTurnSummary {
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CodexStreamEvent {
    AgentMessage { text: String, is_partial: bool },
    Notice(String),
    PendingInteraction(PendingInteractionSummary),
}

pub struct CodexCli {
    config: CodexConfig,
    backend: CodexBackend,
}

enum CodexBackend {
    Exec,
    AppServer(AppServerClient),
}

impl CodexCli {
    pub fn new(config: CodexConfig) -> Self {
        let backend = if config.use_app_server {
            CodexBackend::AppServer(AppServerClient::new(config.clone()))
        } else {
            CodexBackend::Exec
        };

        Self { config, backend }
    }

    pub async fn run_turn<F, Fut>(
        &self,
        request: CodexRequest,
        mut on_event: F,
    ) -> Result<CodexTurnSummary>
    where
        F: FnMut(CodexStreamEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        match &self.backend {
            CodexBackend::Exec => {
                let mut command = self.build_exec_command(&request)?;
                self.run_command(&mut command, Some(request.prompt), &mut on_event)
                    .await
            }
            CodexBackend::AppServer(client) => client.run_turn(request, on_event).await,
        }
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    pub async fn run_review<F, Fut>(
        &self,
        working_directory: PathBuf,
        mut on_event: F,
    ) -> Result<()>
    where
        F: FnMut(CodexStreamEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        if matches!(self.backend, CodexBackend::AppServer(_)) {
            bail!("review is not supported in app-server mode yet");
        }
        let mut command = self.build_review_command(&working_directory)?;
        let _ = self.run_command(&mut command, None, &mut on_event).await?;
        Ok(())
    }

    pub async fn respond_to_pending(
        &self,
        token: &str,
        action: PendingInteractionAction,
    ) -> Result<()> {
        match &self.backend {
            CodexBackend::Exec => {
                bail!("pending approvals are only available in app-server mode")
            }
            CodexBackend::AppServer(client) => client.respond_to_pending(token, action).await,
        }
    }

    pub async fn list_pending_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingInteractionSummary>> {
        match &self.backend {
            CodexBackend::Exec => Ok(Vec::new()),
            CodexBackend::AppServer(client) => client.list_pending_for_thread(thread_id).await,
        }
    }

    pub fn supports_collaboration_mode(&self) -> bool {
        matches!(self.backend, CodexBackend::AppServer(_))
    }

    async fn run_command<F, Fut>(
        &self,
        command: &mut Command,
        prompt: Option<String>,
        on_event: &mut F,
    ) -> Result<CodexTurnSummary>
    where
        F: FnMut(CodexStreamEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        command.kill_on_drop(true);
        let timeout_secs = self.config.turn_timeout_secs.max(1);
        match timeout(
            Duration::from_secs(timeout_secs),
            self.run_command_inner(command, prompt, on_event),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => bail!("codex timed out after {timeout_secs}s"),
        }
    }

    async fn run_command_inner<F, Fut>(
        &self,
        command: &mut Command,
        prompt: Option<String>,
        on_event: &mut F,
    ) -> Result<CodexTurnSummary>
    where
        F: FnMut(CodexStreamEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let launcher = self.config.launcher.join(" ");
        let working_directory = command
            .as_std()
            .get_current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_owned());

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to spawn codex process (launcher: `{launcher}`, working_directory: `{working_directory}`)"
            )
        })?;

        if let Some(prompt) = prompt {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(prompt.as_bytes())
                    .await
                    .context("failed to write prompt to codex stdin")?;
                stdin
                    .write_all(b"\n")
                    .await
                    .context("failed to finalize prompt for codex stdin")?;
                stdin.shutdown().await.ok();
            }
        }

        let stdout = child
            .stdout
            .take()
            .context("failed to capture codex stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture codex stderr")?;

        let stderr_task = tokio::spawn(async move {
            let mut stderr_lines = BufReader::new(stderr).lines();
            while let Some(line) = stderr_lines.next_line().await? {
                let line = line.trim();
                if !line.is_empty() {
                    warn!("codex stderr: {line}");
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        let mut thread_id = None;
        let mut stdout_lines = BufReader::new(stdout).lines();

        while let Some(line) = stdout_lines.next_line().await? {
            match parse_stdout_event(&line)? {
                Some(ParsedStdoutEvent::ThreadStarted(id)) => thread_id = Some(id),
                Some(ParsedStdoutEvent::AgentMessage { text, is_partial }) => {
                    on_event(CodexStreamEvent::AgentMessage { text, is_partial }).await?;
                }
                Some(ParsedStdoutEvent::Notice(message)) => {
                    warn!("codex event: {message}");
                    on_event(CodexStreamEvent::Notice(message)).await?;
                }
                None => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        debug!("ignored non-event stdout line from codex: {trimmed}");
                    }
                }
            }
        }

        let status = child
            .wait()
            .await
            .context("failed while waiting for codex")?;
        stderr_task.await??;

        if !status.success() {
            bail!("codex exited with status {status}");
        }

        Ok(CodexTurnSummary { thread_id })
    }

    fn build_exec_command(&self, request: &CodexRequest) -> Result<Command> {
        let program = self
            .config
            .launcher
            .first()
            .ok_or_else(|| anyhow!("codex launcher cannot be empty"))?;

        let mut command = Command::new(program);
        for arg in self.config.launcher.iter().skip(1) {
            command.arg(arg);
        }

        command.current_dir(&request.working_directory);
        self.apply_proxy_environment(&mut command);
        self.apply_global_codex_options(&mut command);
        command.arg("exec");

        match &request.session_id {
            Some(session_id) => {
                command.arg("resume");
                command.arg("--json");

                if self.config.skip_git_repo_check {
                    command.arg("--skip-git-repo-check");
                }
                if self.config.full_auto {
                    command.arg("--full-auto");
                }
                if let Some(model) = &self.config.model {
                    command.arg("--model");
                    command.arg(model);
                }

                command.arg(session_id);
                command.arg("-");
            }
            None => {
                command.arg("--json");
                command.arg("--color");
                command.arg("never");

                if self.config.skip_git_repo_check {
                    command.arg("--skip-git-repo-check");
                }
                if self.config.full_auto {
                    command.arg("--full-auto");
                }
                if let Some(model) = &self.config.model {
                    command.arg("--model");
                    command.arg(model);
                }
                for dir in &self.config.additional_writable_dirs {
                    command.arg("--add-dir");
                    command.arg(dir);
                }
                for arg in &self.config.extra_args {
                    command.arg(arg);
                }

                command.arg("-");
            }
        }

        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        Ok(command)
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    fn build_review_command(&self, working_directory: &PathBuf) -> Result<Command> {
        let program = self
            .config
            .launcher
            .first()
            .ok_or_else(|| anyhow!("codex launcher cannot be empty"))?;

        let mut command = Command::new(program);
        for arg in self.config.launcher.iter().skip(1) {
            command.arg(arg);
        }

        command.current_dir(working_directory);
        self.apply_proxy_environment(&mut command);
        self.apply_global_codex_options(&mut command);
        command.arg("exec");
        command.arg("review");
        command.arg("--json");
        command.arg("--uncommitted");

        if let Some(model) = &self.config.model {
            command.arg("--model");
            command.arg(model);
        }
        if self.config.full_auto {
            command.arg("--full-auto");
        }
        if self.config.skip_git_repo_check {
            command.arg("--skip-git-repo-check");
        }
        for arg in &self.config.extra_args {
            command.arg(arg);
        }

        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        Ok(command)
    }

    fn apply_proxy_environment(&self, command: &mut Command) {
        apply_optional_env(command, "HTTP_PROXY", self.config.http_proxy.as_deref());
        apply_optional_env(command, "HTTPS_PROXY", self.config.https_proxy.as_deref());
        apply_optional_env(command, "ALL_PROXY", self.config.all_proxy.as_deref());
        apply_optional_env(command, "NO_PROXY", self.config.no_proxy.as_deref());

        // Some tools on Windows still read lowercase proxy variables.
        apply_optional_env(command, "http_proxy", self.config.http_proxy.as_deref());
        apply_optional_env(command, "https_proxy", self.config.https_proxy.as_deref());
        apply_optional_env(command, "all_proxy", self.config.all_proxy.as_deref());
        apply_optional_env(command, "no_proxy", self.config.no_proxy.as_deref());
    }

    fn apply_global_codex_options(&self, command: &mut Command) {
        if let Some(sandbox) = &self.config.sandbox {
            command.arg("--sandbox");
            command.arg(sandbox);
        }

        if let Some(approval) = &self.config.ask_for_approval {
            command.arg("--ask-for-approval");
            command.arg(approval);
        }

        if self.config.search {
            command.arg("--search");
        }

        if let Some(profile) = &self.config.profile {
            command.arg("--profile");
            command.arg(profile);
        }
    }
}

pub(crate) fn apply_optional_env(command: &mut Command, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        if !value.trim().is_empty() {
            command.env(key, value);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedStdoutEvent {
    ThreadStarted(String),
    AgentMessage { text: String, is_partial: bool },
    Notice(String),
}

fn parse_stdout_event(line: &str) -> Result<Option<ParsedStdoutEvent>> {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let Some(event_type) = value.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };

    match event_type {
        "thread.started" => Ok(value
            .get("thread_id")
            .and_then(Value::as_str)
            .map(|id| ParsedStdoutEvent::ThreadStarted(id.to_owned()))),
        "item.completed" => {
            let Some(item) = value.get("item") else {
                return Ok(None);
            };
            match item.get("type").and_then(Value::as_str) {
                Some("agent_message") => Ok(item.get("text").and_then(Value::as_str).map(|text| {
                    ParsedStdoutEvent::AgentMessage {
                        text: text.to_owned(),
                        is_partial: false,
                    }
                })),
                Some("error") => Ok(item
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|message| ParsedStdoutEvent::Notice(message.to_owned()))),
                _ => Ok(None),
            }
        }
        "item.delta" => {
            let item_is_agent_message = value
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                == Some("agent_message");

            if !item_is_agent_message {
                return Ok(None);
            }

            Ok(
                extract_delta_text(&value).map(|text| ParsedStdoutEvent::AgentMessage {
                    text,
                    is_partial: true,
                }),
            )
        }
        "error" => Ok(value
            .get("message")
            .and_then(Value::as_str)
            .map(|message| ParsedStdoutEvent::Notice(message.to_owned()))),
        _ => Ok(None),
    }
}

fn extract_delta_text(value: &Value) -> Option<String> {
    for candidate in [
        value.get("delta"),
        value.get("text_delta"),
        value.get("text"),
        value.get("item").and_then(|item| item.get("delta")),
        value.get("item").and_then(|item| item.get("text_delta")),
        value.get("item").and_then(|item| item.get("text")),
    ] {
        if let Some(candidate) = candidate {
            if let Some(text) = flatten_text(candidate) {
                return Some(text);
            }
        }
    }
    None
}

fn flatten_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.to_owned()),
        Value::Object(map) => {
            for key in ["text", "delta", "content"] {
                if let Some(candidate) = map.get(key) {
                    if let Some(text) = flatten_text(candidate) {
                        return Some(text);
                    }
                }
            }
            None
        }
        Value::Array(values) => {
            let combined = values
                .iter()
                .filter_map(flatten_text)
                .collect::<Vec<_>>()
                .join("");
            if combined.is_empty() {
                None
            } else {
                Some(combined)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_stdout_event, ParsedStdoutEvent};

    #[test]
    fn parses_thread_started_event() {
        let line = r#"{"type":"thread.started","thread_id":"abc"}"#;
        assert_eq!(
            parse_stdout_event(line).unwrap(),
            Some(ParsedStdoutEvent::ThreadStarted("abc".to_owned()))
        );
    }

    #[test]
    fn parses_completed_agent_message() {
        let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}"#;
        assert_eq!(
            parse_stdout_event(line).unwrap(),
            Some(ParsedStdoutEvent::AgentMessage {
                text: "hello".to_owned(),
                is_partial: false,
            })
        );
    }

    #[test]
    fn parses_delta_agent_message() {
        let line =
            r#"{"type":"item.delta","item":{"type":"agent_message","delta":{"text":"hel"}}}"#;
        assert_eq!(
            parse_stdout_event(line).unwrap(),
            Some(ParsedStdoutEvent::AgentMessage {
                text: "hel".to_owned(),
                is_partial: true,
            })
        );
    }
}
