use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::fs;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::codex::{CodexCli, CodexRequest, CodexStreamEvent, CodexTurnSummary};
use crate::im::ImAdapter;
use crate::model::{
    conversation_key, ConversationState, InboundMessage, OutboundMessage, OutboundMessageKind,
    ThreadRecord,
};
use crate::session_store::SessionStore;

pub struct Gateway {
    adapter: Arc<dyn ImAdapter>,
    codex: Arc<CodexCli>,
    session_store: Arc<SessionStore>,
    default_working_directory: PathBuf,
    conversation_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Gateway {
    pub fn new(
        adapter: Arc<dyn ImAdapter>,
        codex: Arc<CodexCli>,
        session_store: Arc<SessionStore>,
        default_working_directory: PathBuf,
    ) -> Self {
        Self {
            adapter,
            codex,
            session_store,
            default_working_directory,
            conversation_locks: Mutex::new(HashMap::new()),
        }
    }

    pub async fn run(self) -> Result<()> {
        let gateway = Arc::new(self);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(64);
        let adapter = gateway.adapter.clone();
        let adapter_task = tokio::spawn(async move { adapter.run(inbound_tx).await });
        let mut tasks = JoinSet::new();
        let mut shutting_down = false;

        loop {
            tokio::select! {
                inbound = inbound_rx.recv() => {
                    match inbound {
                        Some(message) => {
                            let gateway = gateway.clone();
                            tasks.spawn(async move {
                                let adapter_name = message.adapter.clone();
                                let conversation_id = message.conversation_id.clone();
                                if let Err(err) = gateway.clone().handle_message(message).await {
                                    error!("failed to process inbound message: {err:#}");
                                    let _ = gateway
                                        .adapter
                                        .send(OutboundMessage {
                                            adapter: adapter_name,
                                            conversation_id,
                                            text: format!("Codex failed: {err}"),
                                            is_partial: false,
                                            kind: OutboundMessageKind::Notice,
                                        })
                                        .await;
                                }
                            });
                        }
                        None => break,
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("received Ctrl+C, shutting down gateway");
                    shutting_down = true;
                    break;
                }
            }
        }

        if shutting_down {
            tasks.abort_all();
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(inner) => inner?,
                Err(err) if err.is_cancelled() => {}
                Err(err) => return Err(err.into()),
            }
        }

        if shutting_down {
            adapter_task.abort();
        }

        match adapter_task.await {
            Ok(inner) => inner?,
            Err(err) if err.is_cancelled() => {}
            Err(err) => return Err(err.into()),
        }

        Ok(())
    }

    async fn handle_message(self: Arc<Self>, inbound: InboundMessage) -> Result<()> {
        let key = conversation_key(&inbound.adapter, &inbound.conversation_id);
        let lock = self.conversation_lock(&key).await;
        let _guard = lock.lock().await;

        info!(
            adapter = inbound.adapter,
            conversation_id = inbound.conversation_id,
            sender = inbound.sender_id.as_deref().unwrap_or("unknown"),
            "forwarding inbound message to codex"
        );

        let adapter_name = inbound.adapter.clone();
        let conversation_id = inbound.conversation_id.clone();
        let adapter = self.adapter.clone();
        let trimmed_text = inbound.text.trim();

        let mut state = self.load_state(&key).await;

        if let Some(command) = parse_management_command(trimmed_text) {
            self.handle_management_command(
                command,
                &key,
                &mut state,
                &adapter_name,
                &conversation_id,
            )
            .await?;
            return Ok(());
        }

        if trimmed_text.starts_with("/feishu-edit-test") {
            adapter
                .send(OutboundMessage {
                    adapter: adapter_name.clone(),
                    conversation_id: conversation_id.clone(),
                    text: "Feishu edit test: step 1/3. This message should be edited in place."
                        .to_owned(),
                    is_partial: true,
                    kind: OutboundMessageKind::Status,
                })
                .await?;

            sleep(Duration::from_secs(2)).await;
            adapter
                .send(OutboundMessage {
                    adapter: adapter_name.clone(),
                    conversation_id: conversation_id.clone(),
                    text: "Feishu edit test: step 2/3. If you see a new message, in-place update is not working yet."
                        .to_owned(),
                    is_partial: false,
                    kind: OutboundMessageKind::Notice,
                })
                .await?;

            sleep(Duration::from_secs(2)).await;
            adapter
                .send(OutboundMessage {
                    adapter: adapter_name,
                    conversation_id,
                    text: "Feishu edit test: step 3/3 complete. If the same message changed twice, editing works."
                        .to_owned(),
                    is_partial: false,
                    kind: OutboundMessageKind::Agent,
                })
                .await?;

            return Ok(());
        }

        let current_thread = state
            .current_thread_record()
            .cloned()
            .ok_or_else(|| anyhow!("no current thread is selected"))?;
        let working_directory =
            effective_working_directory(&current_thread, &self.default_working_directory);

        if trimmed_text.starts_with("/review") {
            self.send_notice(
                &adapter_name,
                &conversation_id,
                format!(
                    "`/review` is currently disabled. Current thread `{}` is at `{}`.",
                    current_thread.alias,
                    working_directory.display()
                ),
            )
            .await?;
        } else {
            adapter
                .send(OutboundMessage {
                    adapter: adapter_name.clone(),
                    conversation_id: conversation_id.clone(),
                    text: format!(
                        "Codex is thinking in thread `{}` at `{}`...",
                        current_thread.alias,
                        working_directory.display()
                    ),
                    is_partial: true,
                    kind: OutboundMessageKind::Status,
                })
                .await?;

            let prompt = inbound.text;
            let summary = match run_turn_once(
                self.codex.clone(),
                current_thread.codex_thread_id.clone(),
                prompt.clone(),
                working_directory.clone(),
                adapter.clone(),
                adapter_name.clone(),
                conversation_id.clone(),
            )
            .await
            {
                Ok(summary) => summary,
                Err(err)
                    if should_retry_with_fresh_session(
                        current_thread.codex_thread_id.as_ref(),
                        &err,
                    ) =>
                {
                    adapter
                        .send(OutboundMessage {
                            adapter: adapter_name.clone(),
                            conversation_id: conversation_id.clone(),
                            text: "Previous Codex session stalled. Starting a fresh session..."
                                .to_owned(),
                            is_partial: false,
                            kind: OutboundMessageKind::Notice,
                        })
                        .await?;

                    run_turn_once(
                        self.codex.clone(),
                        None,
                        prompt,
                        working_directory,
                        adapter.clone(),
                        adapter_name.clone(),
                        conversation_id.clone(),
                    )
                    .await?
                }
                Err(err) => return Err(err),
            };

            if let Some(thread_id) = summary.thread_id {
                if let Some(thread) = state.current_thread_record_mut() {
                    thread.codex_thread_id = Some(thread_id);
                }
                self.session_store.upsert(key, state).await?;
            }
        }

        Ok(())
    }

    async fn handle_management_command(
        &self,
        command: ManagementCommand,
        key: &str,
        state: &mut ConversationState,
        adapter_name: &str,
        conversation_id: &str,
    ) -> Result<()> {
        match command {
            ManagementCommand::ListThreads => {
                let message =
                    format_threads_message(state, &self.default_working_directory);
                self.send_notice(adapter_name, conversation_id, message).await?;
            }
            ManagementCommand::NewThread(alias) => {
                let alias = alias.unwrap_or_else(|| next_thread_alias(state));
                validate_thread_alias(&alias)?;

                if state.threads.contains_key(&alias) {
                    return Err(anyhow!("thread `{alias}` already exists"));
                }

                let inherited_directory = state
                    .current_thread_record()
                    .map(|thread| {
                        effective_working_directory(thread, &self.default_working_directory)
                    })
                    .unwrap_or_else(|| self.default_working_directory.clone());

                state.threads.insert(
                    alias.clone(),
                    ThreadRecord {
                        alias: alias.clone(),
                        codex_thread_id: None,
                        working_directory: Some(inherited_directory.clone()),
                    },
                );
                state.current_thread = alias.clone();
                self.session_store
                    .upsert(key.to_owned(), state.clone())
                    .await?;

                self.send_notice(
                    adapter_name,
                    conversation_id,
                    format!(
                        "Created thread `{alias}` and switched to it.\nWorking directory: `{}`",
                        inherited_directory.display()
                    ),
                )
                .await?;
            }
            ManagementCommand::UseThread(alias) => {
                if !state.threads.contains_key(&alias) {
                    return Err(anyhow!("thread `{alias}` does not exist"));
                }

                state.current_thread = alias.clone();
                self.session_store
                    .upsert(key.to_owned(), state.clone())
                    .await?;

                let thread = state
                    .current_thread_record()
                    .ok_or_else(|| anyhow!("failed to load current thread after switching"))?;
                let working_directory =
                    effective_working_directory(thread, &self.default_working_directory);

                self.send_notice(
                    adapter_name,
                    conversation_id,
                    format!(
                        "Switched to thread `{alias}`.\nWorking directory: `{}`\nCodex session: {}",
                        working_directory.display(),
                        thread
                            .codex_thread_id
                            .as_deref()
                            .map(short_session_id)
                            .unwrap_or("new")
                    ),
                )
                .await?;
            }
            ManagementCommand::ChangeDirectory(path) => {
                let current_directory = state
                    .current_thread_record()
                    .map(|thread| {
                        effective_working_directory(thread, &self.default_working_directory)
                    })
                    .unwrap_or_else(|| self.default_working_directory.clone());
                let resolved = resolve_directory(&current_directory, &path).await?;

                if let Some(thread) = state.current_thread_record_mut() {
                    thread.working_directory = Some(resolved.clone());
                }
                self.session_store
                    .upsert(key.to_owned(), state.clone())
                    .await?;

                self.send_notice(
                    adapter_name,
                    conversation_id,
                    format!(
                        "Updated working directory for thread `{}` to `{}`",
                        state.current_thread,
                        resolved.display()
                    ),
                )
                .await?;
            }
            ManagementCommand::PrintWorkingDirectory => {
                let thread = state
                    .current_thread_record()
                    .ok_or_else(|| anyhow!("no current thread is selected"))?;
                let working_directory =
                    effective_working_directory(thread, &self.default_working_directory);

                self.send_notice(
                    adapter_name,
                    conversation_id,
                    format!(
                        "Current thread: `{}`\nWorking directory: `{}`\nCodex session: {}",
                        thread.alias,
                        working_directory.display(),
                        thread
                            .codex_thread_id
                            .as_deref()
                            .map(short_session_id)
                            .unwrap_or("new")
                    ),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn send_notice(
        &self,
        adapter_name: &str,
        conversation_id: &str,
        text: String,
    ) -> Result<()> {
        self.adapter
            .send(OutboundMessage {
                adapter: adapter_name.to_owned(),
                conversation_id: conversation_id.to_owned(),
                text,
                is_partial: false,
                kind: OutboundMessageKind::CommandResult,
            })
            .await
    }

    async fn load_state(&self, key: &str) -> ConversationState {
        let mut state = self
            .session_store
            .get(key)
            .await
            .unwrap_or_else(|| ConversationState::with_default_thread(self.default_working_directory.clone()));
        ensure_state_is_usable(&mut state, &self.default_working_directory);
        state
    }

    async fn conversation_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.conversation_locks.lock().await;
        locks
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[derive(Debug, Clone)]
enum ManagementCommand {
    ListThreads,
    NewThread(Option<String>),
    UseThread(String),
    ChangeDirectory(String),
    PrintWorkingDirectory,
}

fn parse_management_command(text: &str) -> Option<ManagementCommand> {
    if text == "/threads" {
        return Some(ManagementCommand::ListThreads);
    }
    if text == "/pwd" || text == "/cwd" || text == "/thread current" {
        return Some(ManagementCommand::PrintWorkingDirectory);
    }
    if let Some(path) = text.strip_prefix("/cd ") {
        let path = path.trim();
        if !path.is_empty() {
            return Some(ManagementCommand::ChangeDirectory(path.to_owned()));
        }
    }
    if text == "/thread" || text == "/thread list" {
        return Some(ManagementCommand::ListThreads);
    }
    if let Some(alias) = text.strip_prefix("/thread new") {
        let alias = alias.trim();
        return Some(ManagementCommand::NewThread(
            (!alias.is_empty()).then(|| alias.to_owned()),
        ));
    }
    if let Some(alias) = text.strip_prefix("/thread use ") {
        let alias = alias.trim();
        if !alias.is_empty() {
            return Some(ManagementCommand::UseThread(alias.to_owned()));
        }
    }
    None
}

fn ensure_state_is_usable(state: &mut ConversationState, default_working_directory: &Path) {
    if state.threads.is_empty() {
        *state = ConversationState::with_default_thread(default_working_directory.to_path_buf());
        return;
    }

    if !state.threads.contains_key(&state.current_thread) {
        state.current_thread = state
            .threads
            .keys()
            .min()
            .cloned()
            .unwrap_or_else(|| "main".to_owned());
    }
}

fn effective_working_directory(thread: &ThreadRecord, default_working_directory: &Path) -> PathBuf {
    thread
        .working_directory
        .clone()
        .unwrap_or_else(|| default_working_directory.to_path_buf())
}

fn next_thread_alias(state: &ConversationState) -> String {
    let mut index = 2;
    loop {
        let candidate = format!("thread-{index}");
        if !state.threads.contains_key(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn validate_thread_alias(alias: &str) -> Result<()> {
    if alias.trim().is_empty() {
        return Err(anyhow!("thread name cannot be empty"));
    }
    if alias.chars().any(char::is_whitespace) {
        return Err(anyhow!("thread name cannot contain spaces"));
    }
    Ok(())
}

async fn resolve_directory(current_directory: &Path, raw_path: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(raw_path);
    let combined = if candidate.is_absolute() {
        candidate
    } else {
        current_directory.join(candidate)
    };

    let metadata = fs::metadata(&combined)
        .await
        .with_context(|| format!("working directory `{}` does not exist", combined.display()))?;
    if !metadata.is_dir() {
        return Err(anyhow!("`{}` is not a directory", combined.display()));
    }

    Ok(fs::canonicalize(&combined).await.unwrap_or(combined))
}

fn format_threads_message(
    state: &ConversationState,
    default_working_directory: &Path,
) -> String {
    let mut aliases = state.threads.keys().cloned().collect::<Vec<_>>();
    aliases.sort();

    let mut lines = vec!["Known threads:".to_owned()];
    for alias in aliases {
        if let Some(thread) = state.threads.get(&alias) {
            let marker = if alias == state.current_thread { "*" } else { "-" };
            let working_directory = effective_working_directory(thread, default_working_directory);
            let session = thread
                .codex_thread_id
                .as_deref()
                .map(short_session_id)
                .unwrap_or("new");
            lines.push(format!(
                "{marker} `{}` | cwd=`{}` | session={session}",
                thread.alias,
                working_directory.display()
            ));
        }
    }

    lines.push("Commands: `/threads`, `/thread new <name>`, `/thread use <name>`, `/cd <path>`, `/pwd`".to_owned());
    lines.join("\n")
}

fn short_session_id(session_id: &str) -> &str {
    let end = session_id.len().min(8);
    &session_id[..end]
}

async fn forward_codex_event(
    adapter: Arc<dyn ImAdapter>,
    adapter_name: String,
    conversation_id: String,
    event: CodexStreamEvent,
) -> Result<()> {
    match event {
        CodexStreamEvent::AgentMessage { text, is_partial } => {
            adapter
                .send(OutboundMessage {
                    adapter: adapter_name,
                    conversation_id,
                    text,
                    is_partial,
                    kind: OutboundMessageKind::Agent,
                })
                .await?;
        }
        CodexStreamEvent::Notice(message) => {
            adapter
                .send(OutboundMessage {
                    adapter: adapter_name,
                    conversation_id,
                    text: format_notice_message(&message),
                    is_partial: false,
                    kind: OutboundMessageKind::Notice,
                })
                .await?;
        }
    }
    Ok(())
}

fn should_retry_with_fresh_session(existing_session: Option<&String>, err: &anyhow::Error) -> bool {
    if existing_session.is_none() {
        return false;
    }

    let message = err.to_string().to_ascii_lowercase();
    message.contains("timed out")
        || message.contains("timeout")
        || message.contains("exited with status")
}

async fn run_turn_once(
    codex: Arc<CodexCli>,
    session_id: Option<String>,
    prompt: String,
    working_directory: PathBuf,
    adapter: Arc<dyn ImAdapter>,
    adapter_name: String,
    conversation_id: String,
) -> Result<CodexTurnSummary> {
    codex
        .run_turn(
            CodexRequest {
                session_id,
                prompt,
                working_directory,
            },
            move |event| {
                let adapter = adapter.clone();
                let adapter_name = adapter_name.clone();
                let conversation_id = conversation_id.clone();
                async move { forward_codex_event(adapter, adapter_name, conversation_id, event).await }
            },
        )
        .await
}

fn format_notice_message(message: &str) -> String {
    if message.starts_with("Reconnecting...") {
        return format!("Codex is reconnecting: {message}");
    }

    if message.starts_with("Codex failed:") {
        return message.to_owned();
    }

    format!("Codex status: {message}")
}
