use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::fs;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::codex::{
    CodexCli, CodexRequest, CodexStreamEvent, CodexTurnSummary, PendingInteractionAction,
};
use crate::im::ImAdapter;
use crate::model::{
    conversation_key, CollaborationModePreset, ConversationState, InboundMessage, OutboundMessage,
    OutboundMessageKind, PendingInteractionKind, PendingInteractionSummary, ThreadRecord,
};
use crate::session_store::SessionStore;

pub struct Gateway {
    adapter: Arc<dyn ImAdapter>,
    codex: Arc<CodexCli>,
    session_store: Arc<SessionStore>,
    default_working_directory: PathBuf,
    conversation_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    active_conversations: Mutex<HashSet<String>>,
    user_input_drafts: Mutex<HashMap<String, serde_json::Map<String, serde_json::Value>>>,
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
            active_conversations: Mutex::new(HashSet::new()),
            user_input_drafts: Mutex::new(HashMap::new()),
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
                                            pending_interaction: None,
                                            dismiss_pending_token: None,
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
                Ok(()) => {}
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
        let trimmed_text = inbound.text.trim();

        info!(
            adapter = inbound.adapter,
            conversation_id = inbound.conversation_id,
            sender = inbound.sender_id.as_deref().unwrap_or("unknown"),
            text = inbound.text.as_str(),
            trimmed_text,
            "gateway received inbound message"
        );

        if let Some(command) = parse_interaction_command(trimmed_text) {
            return self
                .handle_interaction_command(
                    command,
                    &key,
                    &inbound.adapter,
                    &inbound.conversation_id,
                )
                .await;
        }

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

        let mut state = self.load_state(&key).await;

        if let Some(command) = parse_management_command(trimmed_text) {
            if self.is_conversation_active(&key).await {
                self.send_notice(
                    &adapter_name,
                    &conversation_id,
                    "Codex is already busy in this conversation. Wait for the current turn to finish, or respond with:\n\n```text\n/approve\n/deny\n/cancel\n/reply your answer\n/pending\n```".to_owned(),
                )
                .await?;
                return Ok(());
            }
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

        if self.is_conversation_active(&key).await {
            self.send_notice(
                &adapter_name,
                &conversation_id,
                "Codex is already working in this conversation. Wait for the current turn to finish, or answer the pending request with:\n\n```text\n/approve\n/deny\n/cancel\n/reply your answer\n/pending\n```".to_owned(),
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
                    pending_interaction: None,
                    dismiss_pending_token: None,
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
                    pending_interaction: None,
                    dismiss_pending_token: None,
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
                    pending_interaction: None,
                    dismiss_pending_token: None,
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
        drop(_guard);

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
                    pending_interaction: None,
                    dismiss_pending_token: None,
                })
                .await?;
            self.mark_conversation_active(&key).await;

            let prompt = inbound.text;
            let turn_result = async {
                match run_turn_once(
                    self.codex.clone(),
                    current_thread.codex_thread_id.clone(),
                    prompt.clone(),
                    working_directory.clone(),
                    current_thread.collaboration_mode.clone(),
                    adapter.clone(),
                    adapter_name.clone(),
                    conversation_id.clone(),
                )
                .await
                {
                    Ok(summary) => Ok(summary),
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
                                pending_interaction: None,
                                dismiss_pending_token: None,
                            })
                            .await?;

                        run_turn_once(
                            self.codex.clone(),
                            None,
                            prompt,
                            working_directory,
                            current_thread.collaboration_mode.clone(),
                            adapter.clone(),
                            adapter_name.clone(),
                            conversation_id.clone(),
                        )
                        .await
                    }
                    Err(err) => Err(err),
                }
            }
            .await;
            self.mark_conversation_inactive(&key).await;

            let summary = turn_result?;

            if let Some(thread_id) = summary.thread_id {
                if let Some(thread) = state.current_thread_record_mut() {
                    thread.codex_thread_id = Some(thread_id);
                }
                self.session_store.upsert(key, state).await?;
            };
        }

        Ok(())
    }

    async fn handle_interaction_command(
        &self,
        command: InteractionCommand,
        key: &str,
        adapter_name: &str,
        conversation_id: &str,
    ) -> Result<()> {
        let state = self.load_state(key).await;
        let current_thread = state
            .current_thread_record()
            .ok_or_else(|| anyhow!("no current thread is selected"))?;
        let Some(thread_id) = current_thread.codex_thread_id.as_deref() else {
            self.send_notice(
                adapter_name,
                conversation_id,
                "The current thread does not have an active Codex thread id yet.".to_owned(),
            )
            .await?;
            return Ok(());
        };

        match command {
            InteractionCommand::Pending => {
                let pending = self.codex.list_pending_for_thread(thread_id).await?;
                let text = format_pending_list(&pending);
                self.send_notice(adapter_name, conversation_id, text)
                    .await?;
            }
            InteractionCommand::Approve(target) => {
                let pending = self.codex.list_pending_for_thread(thread_id).await?;
                let summary = resolve_pending_target(&pending, &target)?;
                let token = summary.token.clone();
                self.clear_user_input_draft(key, &token).await;
                self.codex
                    .respond_to_pending(&token, PendingInteractionAction::Approve)
                    .await?;
                self.send_command_result(
                    adapter_name,
                    conversation_id,
                    format!("Approved Codex request `{token}`. Codex is continuing in this conversation."),
                    Some(token),
                )
                .await?;
            }
            InteractionCommand::ApproveForSession(target) => {
                let pending = self.codex.list_pending_for_thread(thread_id).await?;
                let summary = resolve_pending_target(&pending, &target)?;
                let token = summary.token.clone();
                self.clear_user_input_draft(key, &token).await;
                self.codex
                    .respond_to_pending(&token, PendingInteractionAction::ApproveForSession)
                    .await?;
                self.send_command_result(
                    adapter_name,
                    conversation_id,
                    format!("Approved Codex request `{token}` for the rest of the session. Codex is continuing in this conversation."),
                    Some(token),
                )
                .await?;
            }
            InteractionCommand::Deny(target) => {
                let pending = self.codex.list_pending_for_thread(thread_id).await?;
                let summary = resolve_pending_target(&pending, &target)?;
                let token = summary.token.clone();
                self.clear_user_input_draft(key, &token).await;
                self.codex
                    .respond_to_pending(&token, PendingInteractionAction::Deny)
                    .await?;
                self.send_command_result(
                    adapter_name,
                    conversation_id,
                    format!("Denied Codex request `{token}`."),
                    Some(token),
                )
                .await?;
            }
            InteractionCommand::Cancel(target) => {
                let pending = self.codex.list_pending_for_thread(thread_id).await?;
                let summary = resolve_pending_target(&pending, &target)?;
                let token = summary.token.clone();
                self.clear_user_input_draft(key, &token).await;
                self.codex
                    .respond_to_pending(&token, PendingInteractionAction::Cancel)
                    .await?;
                self.send_command_result(
                    adapter_name,
                    conversation_id,
                    format!("Cancelled Codex request `{token}`."),
                    Some(token),
                )
                .await?;
            }
            InteractionCommand::ReplyOther {
                target,
                question_id,
            } => {
                let pending = self.codex.list_pending_for_thread(thread_id).await?;
                let summary = resolve_pending_target(&pending, &target)?;
                let ordinal = pending
                    .iter()
                    .position(|item| item.token == summary.token)
                    .map(|index| index + 1);

                let text = match &summary.kind {
                    PendingInteractionKind::UserInputRequest { questions }
                        if questions.len() == 1 =>
                    {
                        if pending.len() == 1 {
                            "Send your custom answer like this:\n\n```text\n/reply your answer\n```"
                                .to_owned()
                        } else if let Some(ordinal) = ordinal {
                            format!("Send your custom answer like this:\n\n```text\n/reply {ordinal} your answer\n```")
                        } else {
                            format!(
                                "Send your custom answer like this:\n\n```text\n/reply {} your answer\n```",
                                summary.token
                            )
                        }
                    }
                    PendingInteractionKind::UserInputRequest { .. } => {
                        let question_id = question_id.as_deref().unwrap_or("question_id");
                        if pending.len() == 1 {
                            format!(
                                "Send your custom answer like this:\n\n```text\n/reply {question_id} your answer\n```"
                            )
                        } else if let Some(ordinal) = ordinal {
                            format!(
                                "Send your custom answer like this:\n\n```text\n/reply {ordinal} {question_id} your answer\n```"
                            )
                        } else {
                            format!(
                                "Send your custom answer like this:\n\n```text\n/reply {} {question_id} your answer\n```",
                                summary.token
                            )
                        }
                    }
                    _ => "This pending Codex request does not accept free-form input.".to_owned(),
                };

                self.send_notice(adapter_name, conversation_id, text)
                    .await?;
            }
            InteractionCommand::ReplyText { target, text } => {
                let pending = self.codex.list_pending_for_thread(thread_id).await?;
                let summary = resolve_pending_target(&pending, &target)?;
                let token = summary.token.clone();
                if let PendingInteractionKind::UserInputRequest { questions } = &summary.kind {
                    if questions.len() > 1 {
                        let Some((question_id, answer_text)) =
                            parse_multi_question_reply_text(&text, questions)
                        else {
                            self.send_notice(
                                adapter_name,
                                conversation_id,
                                format!(
                                    "This request still has multiple questions. Reply like this:\n\n```text\n/reply <question_id> your answer\n```\n\nIf there are multiple pending requests, use:\n\n```text\n/reply <item-number> <question_id> your answer\n```"
                                ),
                            )
                            .await?;
                            return Ok(());
                        };

                        let value = serde_json::json!({
                            question_id: [answer_text]
                        });

                        let merged = merge_multi_question_answers(
                            self.take_user_input_draft(key, &token).await,
                            questions,
                            value,
                        )?;
                        let is_complete = multi_question_answers_complete(questions, &merged);
                        self.store_user_input_draft(key, &token, merged.clone())
                            .await;

                        if is_complete {
                            self.clear_user_input_draft(key, &token).await;
                            self.codex
                                .respond_to_pending(
                                    &token,
                                    PendingInteractionAction::ReplyJson(serde_json::Value::Object(
                                        merged,
                                    )),
                                )
                                .await?;
                            self.send_command_result(
                                adapter_name,
                                conversation_id,
                                format!("Sent your response to Codex request `{token}`. Codex is continuing in this conversation."),
                                Some(token),
                            )
                            .await?;
                        } else {
                            let remaining = remaining_multi_question_ids(questions, &merged);
                            self.send_notice(
                                adapter_name,
                                conversation_id,
                                format!(
                                    "Saved your answer for Codex request `{token}`.\n\nRemaining question ids:\n\n```text\n{}\n```",
                                    remaining.join("\n")
                                ),
                            )
                            .await?;
                        }
                        return Ok(());
                    }
                }

                let action = if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    PendingInteractionAction::ReplyJson(value)
                } else {
                    PendingInteractionAction::ReplyText(text)
                };
                self.clear_user_input_draft(key, &token).await;
                self.codex.respond_to_pending(&token, action).await?;
                self.send_command_result(
                    adapter_name,
                    conversation_id,
                    format!("Sent your response to Codex request `{token}`. Codex is continuing in this conversation."),
                    Some(token),
                )
                .await?;
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
                let message = format_threads_message(state, &self.default_working_directory);
                self.send_notice(adapter_name, conversation_id, message)
                    .await?;
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
                let inherited_mode = state
                    .current_thread_record()
                    .and_then(|thread| thread.collaboration_mode.clone());

                state.threads.insert(
                    alias.clone(),
                    ThreadRecord {
                        alias: alias.clone(),
                        codex_thread_id: None,
                        working_directory: Some(inherited_directory.clone()),
                        collaboration_mode: inherited_mode.clone(),
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
                        "Created thread `{alias}` and switched to it.\nWorking directory: `{}`\nMode: `{}`",
                        inherited_directory.display(),
                        inherited_mode
                            .as_ref()
                            .map(CollaborationModePreset::display_name)
                            .unwrap_or("codex-default")
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
                        "Switched to thread `{alias}`.\nWorking directory: `{}`\nMode: `{}`\nCodex session: {}",
                        working_directory.display(),
                        describe_thread_mode(thread),
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
                        "Current thread: `{}`\nWorking directory: `{}`\nMode: `{}`\nCodex session: {}",
                        thread.alias,
                        working_directory.display(),
                        describe_thread_mode(thread),
                        thread
                            .codex_thread_id
                            .as_deref()
                            .map(short_session_id)
                            .unwrap_or("new")
                    ),
                )
                .await?;
            }
            ManagementCommand::ShowMode => {
                let thread = state
                    .current_thread_record()
                    .ok_or_else(|| anyhow!("no current thread is selected"))?;
                let backend_note = if self.codex.supports_collaboration_mode() {
                    "This mode will be sent to Codex on the next turn."
                } else {
                    "This gateway is not using app-server mode, so the setting is only stored locally."
                };

                self.send_notice(
                    adapter_name,
                    conversation_id,
                    format!(
                        "Current mode for thread `{}`: `{}`.\n{backend_note}",
                        thread.alias,
                        describe_thread_mode(thread)
                    ),
                )
                .await?;
            }
            ManagementCommand::SetMode(mode) => {
                let alias = {
                    let thread = state
                        .current_thread_record_mut()
                        .ok_or_else(|| anyhow!("no current thread is selected"))?;
                    thread.collaboration_mode = Some(mode.clone());
                    thread.alias.clone()
                };

                self.session_store
                    .upsert(key.to_owned(), state.clone())
                    .await?;

                let backend_note = if self.codex.supports_collaboration_mode() {
                    "It will take effect on the next turn in this thread."
                } else {
                    "This gateway is not using app-server mode, so the setting is only stored locally."
                };

                self.send_notice(
                    adapter_name,
                    conversation_id,
                    format!(
                        "Switched thread `{alias}` to `{}` mode.\n{backend_note}",
                        mode.display_name()
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
            .send(build_notice_message(adapter_name, conversation_id, text))
            .await
    }

    async fn send_command_result(
        &self,
        adapter_name: &str,
        conversation_id: &str,
        text: String,
        dismiss_pending_token: Option<String>,
    ) -> Result<()> {
        self.adapter
            .send(build_command_result_message(
                adapter_name,
                conversation_id,
                text,
                dismiss_pending_token,
            ))
            .await
    }

    async fn take_user_input_draft(
        &self,
        key: &str,
        token: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        self.user_input_drafts
            .lock()
            .await
            .remove(&pending_draft_key(key, token))
            .unwrap_or_default()
    }

    async fn store_user_input_draft(
        &self,
        key: &str,
        token: &str,
        answers: serde_json::Map<String, serde_json::Value>,
    ) {
        self.user_input_drafts
            .lock()
            .await
            .insert(pending_draft_key(key, token), answers);
    }

    async fn clear_user_input_draft(&self, key: &str, token: &str) {
        self.user_input_drafts
            .lock()
            .await
            .remove(&pending_draft_key(key, token));
    }

    async fn load_state(&self, key: &str) -> ConversationState {
        let mut state = self.session_store.get(key).await.unwrap_or_else(|| {
            ConversationState::with_default_thread(self.default_working_directory.clone())
        });
        ensure_state_is_usable(&mut state, &self.default_working_directory);
        state
    }

    async fn is_conversation_active(&self, key: &str) -> bool {
        self.active_conversations.lock().await.contains(key)
    }

    async fn mark_conversation_active(&self, key: &str) {
        self.active_conversations
            .lock()
            .await
            .insert(key.to_owned());
    }

    async fn mark_conversation_inactive(&self, key: &str) {
        self.active_conversations.lock().await.remove(key);
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
    ShowMode,
    SetMode(CollaborationModePreset),
}

#[derive(Debug, Clone)]
enum InteractionCommand {
    Pending,
    Approve(PendingTarget),
    ApproveForSession(PendingTarget),
    Deny(PendingTarget),
    Cancel(PendingTarget),
    ReplyOther {
        target: PendingTarget,
        question_id: Option<String>,
    },
    ReplyText {
        target: PendingTarget,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingTarget {
    Implicit,
    Ordinal(usize),
    Token(String),
    Last,
}

fn parse_management_command(text: &str) -> Option<ManagementCommand> {
    if text == "/threads" {
        return Some(ManagementCommand::ListThreads);
    }
    if text == "/mode" {
        return Some(ManagementCommand::ShowMode);
    }
    if let Some(raw_mode) = text.strip_prefix("/mode ") {
        let raw_mode = raw_mode.trim().to_ascii_lowercase();
        return match raw_mode.as_str() {
            "plan" => Some(ManagementCommand::SetMode(CollaborationModePreset::Plan)),
            "default" => Some(ManagementCommand::SetMode(CollaborationModePreset::Default)),
            _ => None,
        };
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

fn parse_interaction_command(text: &str) -> Option<InteractionCommand> {
    if text == "/pending" {
        return Some(InteractionCommand::Pending);
    }
    if text == "/approve-session" {
        return Some(InteractionCommand::ApproveForSession(
            PendingTarget::Implicit,
        ));
    }
    if let Some(target) = text.strip_prefix("/approve-session ") {
        let target = target.trim();
        if !target.is_empty() {
            return Some(InteractionCommand::ApproveForSession(parse_pending_target(
                target,
            )));
        }
    }
    if text == "/approve" {
        return Some(InteractionCommand::Approve(PendingTarget::Implicit));
    }
    if let Some(target) = text.strip_prefix("/approve ") {
        let target = target.trim();
        if !target.is_empty() {
            return Some(InteractionCommand::Approve(parse_pending_target(target)));
        }
    }
    if text == "/deny" {
        return Some(InteractionCommand::Deny(PendingTarget::Implicit));
    }
    if let Some(target) = text.strip_prefix("/deny ") {
        let target = target.trim();
        if !target.is_empty() {
            return Some(InteractionCommand::Deny(parse_pending_target(target)));
        }
    }
    if text == "/cancel" {
        return Some(InteractionCommand::Cancel(PendingTarget::Implicit));
    }
    if let Some(target) = text.strip_prefix("/cancel ") {
        let target = target.trim();
        if !target.is_empty() {
            return Some(InteractionCommand::Cancel(parse_pending_target(target)));
        }
    }
    if text == "/reply-other" {
        return Some(InteractionCommand::ReplyOther {
            target: PendingTarget::Implicit,
            question_id: None,
        });
    }
    if let Some(rest) = text.strip_prefix("/reply-other ") {
        let rest = rest.trim();
        if !rest.is_empty() {
            if let Some((target, question_id)) = split_pending_target_and_text(rest) {
                return Some(InteractionCommand::ReplyOther {
                    target,
                    question_id: Some(question_id.to_owned()),
                });
            }
            return Some(InteractionCommand::ReplyOther {
                target: parse_pending_target(rest),
                question_id: None,
            });
        }
    }
    if let Some(rest) = text.strip_prefix("/reply ") {
        let rest = rest.trim();
        if !rest.is_empty() {
            if let Some((target, answer)) = split_pending_target_and_text(rest) {
                return Some(InteractionCommand::ReplyText {
                    target,
                    text: answer.to_owned(),
                });
            }
            return Some(InteractionCommand::ReplyText {
                target: PendingTarget::Implicit,
                text: rest.to_owned(),
            });
        }
    }
    None
}

fn split_pending_target_and_text(text: &str) -> Option<(PendingTarget, &str)> {
    let first_whitespace = text.find(char::is_whitespace)?;
    let target_text = text[..first_whitespace].trim();
    let answer = text[first_whitespace..].trim();
    if target_text.is_empty() || answer.is_empty() {
        return None;
    }
    if is_explicit_pending_target(target_text) {
        Some((parse_pending_target(target_text), answer))
    } else {
        None
    }
}

fn is_explicit_pending_target(text: &str) -> bool {
    let normalized = normalize_pending_target_text(text);
    if normalized.is_empty() {
        return false;
    }

    if normalized.eq_ignore_ascii_case("last")
        || normalized.eq_ignore_ascii_case("latest")
        || normalized.starts_with("req-")
    {
        return true;
    }

    if normalized
        .parse::<usize>()
        .ok()
        .filter(|index| *index > 0)
        .is_some()
    {
        return true;
    }

    parse_chinese_ordinal(normalized).is_some()
}

fn parse_pending_target(text: &str) -> PendingTarget {
    let normalized = normalize_pending_target_text(text);
    if normalized.is_empty() {
        return PendingTarget::Implicit;
    }

    if normalized.eq_ignore_ascii_case("last") || normalized.eq_ignore_ascii_case("latest") {
        return PendingTarget::Last;
    }

    if let Ok(index) = normalized.parse::<usize>() {
        if index > 0 {
            return PendingTarget::Ordinal(index);
        }
    }

    if let Some(index) = parse_chinese_ordinal(normalized) {
        return PendingTarget::Ordinal(index);
    }

    PendingTarget::Token(normalized.to_owned())
}

fn normalize_pending_target_text(text: &str) -> &str {
    text.trim()
        .trim_end_matches(|c: char| matches!(c, '.' | ')' | ',' | ':' | ';'))
        .trim()
}

fn parse_chinese_ordinal(text: &str) -> Option<usize> {
    let candidate = text
        .strip_prefix('\u{7b2c}')
        .unwrap_or(text)
        .strip_suffix("\u{9879}")
        .or_else(|| {
            text.strip_prefix('\u{7b2c}')
                .unwrap_or(text)
                .strip_suffix('\u{4e2a}')
        })
        .or_else(|| {
            text.strip_prefix('\u{7b2c}')
                .unwrap_or(text)
                .strip_suffix('\u{6761}')
        })
        .or_else(|| {
            text.strip_prefix('\u{7b2c}')
                .unwrap_or(text)
                .strip_suffix('\u{9898}')
        })
        .unwrap_or(text.strip_prefix('\u{7b2c}').unwrap_or(text));

    if let Ok(index) = candidate.parse::<usize>() {
        return (index > 0).then_some(index);
    }

    parse_simple_chinese_number(candidate)
}

fn parse_simple_chinese_number(text: &str) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    if text == "\u{5341}" {
        return Some(10);
    }
    if let Some(ones) = text.strip_prefix('\u{5341}') {
        return chinese_digit_value(ones).map(|ones| 10 + ones);
    }
    if let Some(tens) = text.strip_suffix('\u{5341}') {
        return chinese_digit_value(tens).map(|tens| tens * 10);
    }
    if let Some((tens, ones)) = text.split_once('\u{5341}') {
        return Some(chinese_digit_value(tens)? * 10 + chinese_digit_value(ones)?);
    }
    chinese_digit_value(text)
}

fn chinese_digit_value(text: &str) -> Option<usize> {
    match text {
        "\u{4e00}" => Some(1),
        "\u{4e8c}" | "\u{4e24}" => Some(2),
        "\u{4e09}" => Some(3),
        "\u{56db}" => Some(4),
        "\u{4e94}" => Some(5),
        "\u{516d}" => Some(6),
        "\u{4e03}" => Some(7),
        "\u{516b}" => Some(8),
        "\u{4e5d}" => Some(9),
        _ => None,
    }
}
fn resolve_pending_target<'a>(
    pending: &'a [PendingInteractionSummary],
    target: &PendingTarget,
) -> Result<&'a PendingInteractionSummary> {
    if pending.is_empty() {
        return Err(anyhow!(
            "No pending Codex approvals or questions for the current thread."
        ));
    }

    match target {
        PendingTarget::Implicit => {
            if pending.len() == 1 {
                Ok(&pending[0])
            } else {
                Err(anyhow!(
                    "There are {} pending Codex requests right now.\n\n{}",
                    pending.len(),
                    format_pending_list(pending)
                ))
            }
        }
        PendingTarget::Ordinal(index) => pending.get(index - 1).ok_or_else(|| {
            anyhow!(
                "Pending request #{index} does not exist.\n\n{}",
                format_pending_list(pending)
            )
        }),
        PendingTarget::Token(token) => pending
            .iter()
            .find(|summary| summary.token == *token)
            .ok_or_else(|| {
                anyhow!(
                    "No pending Codex request matches token `{token}`.\n\n{}",
                    format_pending_list(pending)
                )
            }),
        PendingTarget::Last => pending.last().ok_or_else(|| {
            anyhow!("No pending Codex approvals or questions for the current thread.")
        }),
    }
}

fn format_pending_list(pending: &[PendingInteractionSummary]) -> String {
    if pending.is_empty() {
        return "No pending Codex approvals or questions for the current thread.".to_owned();
    }

    let mut blocks = vec![
        "Pending Codex requests:".to_owned(),
        "If there is only one pending request, reply with:".to_owned(),
        "```text\n/approve\n/deny\n/cancel\n/reply your answer\n```".to_owned(),
        "If there are multiple pending requests, use the item number shown below:".to_owned(),
        "```text\n/approve 1\n/deny 1\n/cancel 1\n/reply 1 your answer\n```".to_owned(),
    ];
    for (index, summary) in pending.iter().enumerate() {
        let body = format!("{}\n{}", pending_summary_heading(summary), summary.prompt);
        blocks.push(prefix_block(&format!("{}. ", index + 1), &body));
    }
    blocks.join("\n\n")
}

fn pending_summary_heading(summary: &PendingInteractionSummary) -> String {
    match &summary.kind {
        crate::model::PendingInteractionKind::CommandApproval { .. } => {
            format!("Command approval (`{}`)", summary.token)
        }
        crate::model::PendingInteractionKind::FileChangeApproval => {
            format!("File-change approval (`{}`)", summary.token)
        }
        crate::model::PendingInteractionKind::PermissionsApproval { .. } => {
            format!("Additional permissions (`{}`)", summary.token)
        }
        crate::model::PendingInteractionKind::UserInputRequest { questions } => {
            format!(
                "User input request (`{}`) | {} question(s)",
                summary.token,
                questions.len()
            )
        }
    }
}

fn prefix_block(prefix: &str, block: &str) -> String {
    let mut lines = block.lines();
    let Some(first_line) = lines.next() else {
        return prefix.trim_end().to_owned();
    };

    let mut output = format!("{prefix}{first_line}");
    let padding = " ".repeat(prefix.chars().count());
    for line in lines {
        output.push('\n');
        output.push_str(&padding);
        output.push_str(line);
    }
    output
}

fn build_notice_message(
    adapter_name: &str,
    conversation_id: &str,
    text: String,
) -> OutboundMessage {
    OutboundMessage {
        adapter: adapter_name.to_owned(),
        conversation_id: conversation_id.to_owned(),
        text,
        is_partial: false,
        kind: OutboundMessageKind::Notice,
        pending_interaction: None,
        dismiss_pending_token: None,
    }
}

fn build_command_result_message(
    adapter_name: &str,
    conversation_id: &str,
    text: String,
    dismiss_pending_token: Option<String>,
) -> OutboundMessage {
    OutboundMessage {
        adapter: adapter_name.to_owned(),
        conversation_id: conversation_id.to_owned(),
        text,
        is_partial: false,
        kind: OutboundMessageKind::CommandResult,
        pending_interaction: None,
        dismiss_pending_token,
    }
}

fn pending_draft_key(conversation_key: &str, token: &str) -> String {
    format!("{conversation_key}::{token}")
}

fn merge_multi_question_answers(
    mut existing: serde_json::Map<String, serde_json::Value>,
    questions: &[serde_json::Value],
    value: serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let incoming = value
        .as_object()
        .ok_or_else(|| anyhow!("expected a JSON object mapping question ids to answers"))?;

    for (question_id, answer_value) in incoming {
        if !questions.iter().any(|question| {
            question.get("id").and_then(serde_json::Value::as_str) == Some(question_id.as_str())
        }) {
            return Err(anyhow!("unknown question id `{question_id}`"));
        }

        let normalized = if let Some(text) = answer_value.as_str() {
            serde_json::Value::Array(vec![serde_json::Value::String(text.to_owned())])
        } else if let Some(values) = answer_value.as_array() {
            let mut normalized = Vec::with_capacity(values.len());
            for value in values {
                let text = value
                    .as_str()
                    .ok_or_else(|| anyhow!("all answer values must be strings"))?;
                normalized.push(serde_json::Value::String(text.to_owned()));
            }
            serde_json::Value::Array(normalized)
        } else {
            return Err(anyhow!(
                "answer for question `{question_id}` must be a string or string array"
            ));
        };

        existing.insert(question_id.clone(), normalized);
    }

    Ok(existing)
}

fn parse_multi_question_reply_text(
    text: &str,
    questions: &[serde_json::Value],
) -> Option<(String, String)> {
    let trimmed = text.trim();
    let split = trimmed.find(char::is_whitespace)?;
    let question_id = trimmed[..split].trim();
    let answer = trimmed[split..].trim();
    if question_id.is_empty() || answer.is_empty() {
        return None;
    }
    questions
        .iter()
        .any(|question| question.get("id").and_then(serde_json::Value::as_str) == Some(question_id))
        .then(|| (question_id.to_owned(), answer.to_owned()))
}

fn multi_question_answers_complete(
    questions: &[serde_json::Value],
    answers: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    questions.iter().all(|question| {
        question
            .get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(|question_id| answers.get(question_id))
            .and_then(serde_json::Value::as_array)
            .map(|answers| !answers.is_empty())
            .unwrap_or(false)
    })
}

fn remaining_multi_question_ids(
    questions: &[serde_json::Value],
    answers: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    questions
        .iter()
        .filter_map(|question| {
            let question_id = question.get("id").and_then(serde_json::Value::as_str)?;
            let answered = answers
                .get(question_id)
                .and_then(serde_json::Value::as_array)
                .map(|answers| !answers.is_empty())
                .unwrap_or(false);
            (!answered).then(|| question_id.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        parse_interaction_command, parse_management_command, parse_pending_target,
        resolve_pending_target, InteractionCommand, ManagementCommand, PendingTarget,
    };
    use crate::model::{
        CollaborationModePreset, OutboundMessageKind, PendingInteractionKind,
        PendingInteractionSummary,
    };
    use serde_json::json;

    #[test]
    fn parses_implicit_approval_command() {
        let command = parse_interaction_command("/approve").expect("command");
        assert!(matches!(
            command,
            InteractionCommand::Approve(PendingTarget::Implicit)
        ));
    }

    #[test]
    fn parses_ordinal_targets() {
        assert_eq!(parse_pending_target("1"), PendingTarget::Ordinal(1));
        assert_eq!(
            parse_pending_target("\u{7b2c}\u{4e8c}\u{9879}"),
            PendingTarget::Ordinal(2)
        );
        assert_eq!(
            parse_pending_target("\u{7b2c}\u{4e09}\u{4e2a}"),
            PendingTarget::Ordinal(3)
        );
        assert_eq!(parse_pending_target("last"), PendingTarget::Last);
    }

    #[test]
    fn reply_without_target_stays_implicit() {
        let command = parse_interaction_command("/reply 2").expect("command");
        assert!(matches!(
            command,
            InteractionCommand::ReplyText {
                target: PendingTarget::Implicit,
                text,
            } if text == "2"
        ));
    }

    #[test]
    fn reply_with_target_and_answer_uses_ordinal_target() {
        let command = parse_interaction_command(
            "/reply \u{7b2c}\u{4e8c}\u{9879} \u{6211}\u{9009}\u{7b2c}\u{4e8c}\u{4e2a}",
        )
        .expect("command");
        assert!(matches!(
            command,
            InteractionCommand::ReplyText {
                target: PendingTarget::Ordinal(2),
                text,
            } if text == "\u{6211}\u{9009}\u{7b2c}\u{4e8c}\u{4e2a}"
        ));
    }

    #[test]
    fn resolve_implicit_target_requires_single_pending_item() {
        let pending = vec![sample_pending("req-1"), sample_pending("req-2")];
        let err =
            resolve_pending_target(&pending, &PendingTarget::Implicit).expect_err("should fail");
        assert!(err
            .to_string()
            .contains("There are 2 pending Codex requests"));
    }

    #[test]
    fn reply_with_plain_text_sentence_keeps_whole_answer() {
        let command = parse_interaction_command("/reply my custom answer").expect("command");
        assert!(matches!(
            command,
            InteractionCommand::ReplyText {
                target: PendingTarget::Implicit,
                text,
            } if text == "my custom answer"
        ));
    }

    #[test]
    fn reply_with_req_token_keeps_explicit_target() {
        let command = parse_interaction_command("/reply req-3 my custom answer").expect("command");
        assert!(matches!(
            command,
            InteractionCommand::ReplyText {
                target: PendingTarget::Token(token),
                text,
            } if token == "req-3" && text == "my custom answer"
        ));
    }

    #[test]
    fn resolve_last_target_returns_most_recent_item() {
        let pending = vec![sample_pending("req-1"), sample_pending("req-2")];
        let summary = resolve_pending_target(&pending, &PendingTarget::Last).expect("summary");
        assert_eq!(summary.token, "req-2");
    }

    #[test]
    fn parses_mode_query_management_command() {
        assert!(matches!(
            parse_management_command("/mode"),
            Some(ManagementCommand::ShowMode)
        ));
    }

    #[test]
    fn parses_mode_plan_management_command() {
        assert!(matches!(
            parse_management_command("/mode plan"),
            Some(ManagementCommand::SetMode(CollaborationModePreset::Plan))
        ));
    }

    #[test]
    fn parses_reply_other_command() {
        assert!(matches!(
            parse_interaction_command("/reply-other"),
            Some(InteractionCommand::ReplyOther {
                target: PendingTarget::Implicit,
                question_id: None,
            })
        ));
    }

    #[test]
    fn parses_reply_other_with_question_id() {
        assert!(matches!(
            parse_interaction_command("/reply-other req-3 color_choice"),
            Some(InteractionCommand::ReplyOther {
                target: PendingTarget::Token(token),
                question_id: Some(question_id),
            }) if token == "req-3" && question_id == "color_choice"
        ));
    }

    #[test]
    fn merges_multi_question_answers_until_complete() {
        let questions = vec![json!({ "id": "q1" }), json!({ "id": "q2" })];
        let merged = super::merge_multi_question_answers(
            serde_json::Map::new(),
            &questions,
            json!({ "q1": ["A"] }),
        )
        .expect("merge");
        assert!(!super::multi_question_answers_complete(&questions, &merged));
        assert_eq!(
            super::remaining_multi_question_ids(&questions, &merged),
            vec!["q2".to_owned()]
        );
    }

    #[test]
    fn parses_multi_question_reply_text_as_plain_text_pair() {
        let questions = vec![
            json!({ "id": "color_choice" }),
            json!({ "id": "size_choice" }),
        ];

        let parsed =
            super::parse_multi_question_reply_text("color_choice deep navy blue", &questions);
        assert_eq!(
            parsed,
            Some(("color_choice".to_owned(), "deep navy blue".to_owned()))
        );
    }

    #[test]
    fn builds_notice_message_with_notice_kind() {
        let message = super::build_notice_message("feishu", "chat-1", "hello".to_owned());
        assert!(matches!(message.kind, OutboundMessageKind::Notice));
        assert_eq!(message.adapter, "feishu");
        assert_eq!(message.conversation_id, "chat-1");
        assert_eq!(message.text, "hello");
        assert!(message.dismiss_pending_token.is_none());
    }

    #[test]
    fn builds_command_result_message_with_command_result_kind() {
        let message = super::build_command_result_message(
            "feishu",
            "chat-1",
            "done".to_owned(),
            Some("req-1".to_owned()),
        );
        assert!(matches!(message.kind, OutboundMessageKind::CommandResult));
        assert_eq!(message.dismiss_pending_token.as_deref(), Some("req-1"));
    }

    fn sample_pending(token: &str) -> PendingInteractionSummary {
        PendingInteractionSummary {
            token: token.to_owned(),
            kind: PendingInteractionKind::UserInputRequest {
                questions: vec![json!({
                    "id": "choice",
                    "header": "Pick",
                    "question": "Choose one option",
                    "options": [
                        { "label": "One" },
                        { "label": "Two" }
                    ]
                })],
            },
            prompt: format!("Codex requests user input (`{token}`)."),
        }
    }
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

fn format_threads_message(state: &ConversationState, default_working_directory: &Path) -> String {
    let mut aliases = state.threads.keys().cloned().collect::<Vec<_>>();
    aliases.sort();

    let mut lines = vec!["Known threads:".to_owned()];
    for alias in aliases {
        if let Some(thread) = state.threads.get(&alias) {
            let marker = if alias == state.current_thread {
                "*"
            } else {
                "-"
            };
            let working_directory = effective_working_directory(thread, default_working_directory);
            let session = thread
                .codex_thread_id
                .as_deref()
                .map(short_session_id)
                .unwrap_or("new");
            let mode = describe_thread_mode(thread);
            lines.push(format!(
                "{marker} `{}` | cwd=`{}` | mode={mode} | session={session}",
                thread.alias,
                working_directory.display()
            ));
        }
    }

    lines.push("Commands: `/threads`, `/thread new <name>`, `/thread use <name>`, `/cd <path>`, `/pwd`, `/mode`, `/mode plan`, `/mode default`".to_owned());
    lines.join("\n")
}

fn describe_thread_mode(thread: &ThreadRecord) -> &'static str {
    thread
        .collaboration_mode
        .as_ref()
        .map(CollaborationModePreset::display_name)
        .unwrap_or("codex-default")
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
                    pending_interaction: None,
                    dismiss_pending_token: None,
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
                    pending_interaction: None,
                    dismiss_pending_token: None,
                })
                .await?;
        }
        CodexStreamEvent::PendingInteraction(summary) => {
            adapter
                .send(OutboundMessage {
                    adapter: adapter_name,
                    conversation_id,
                    text: summary.prompt.clone(),
                    is_partial: false,
                    kind: OutboundMessageKind::PendingInteraction,
                    pending_interaction: Some(summary),
                    dismiss_pending_token: None,
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
    collaboration_mode: Option<CollaborationModePreset>,
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
                collaboration_mode,
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
