use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::codex::{
    apply_optional_env, CodexRequest, CodexStreamEvent, CodexTurnSummary, PendingInteractionAction,
};
use crate::config::CodexConfig;
use crate::model::{CollaborationModePreset, PendingInteractionKind, PendingInteractionSummary};

const APP_SERVER_SESSION_SOURCE: &str = "appServer";
const APP_SERVER_INIT_TIMEOUT_SECS: u64 = 15;
const APP_SERVER_RPC_TIMEOUT_SECS: u64 = 30;

pub struct AppServerClient {
    config: CodexConfig,
    connection: Mutex<Option<Arc<AppServerConnection>>>,
}

impl AppServerClient {
    pub fn new(config: CodexConfig) -> Self {
        Self {
            config,
            connection: Mutex::new(None),
        }
    }

    pub async fn run_turn<F, Fut>(
        &self,
        request: CodexRequest,
        mut on_event: F,
    ) -> Result<CodexTurnSummary>
    where
        F: FnMut(CodexStreamEvent) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let connection = self.ensure_connection().await?;
        let thread_id = match request.session_id {
            Some(thread_id) => {
                connection
                    .resume_thread(&thread_id, &request.working_directory)
                    .await?
            }
            None => connection.start_thread(&request.working_directory).await?,
        };

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut completion_rx = connection.register_active_turn(thread_id.clone(), event_tx).await?;

        if let Err(err) = connection
            .start_turn(&thread_id, &request.prompt, request.collaboration_mode.as_ref())
            .await
        {
            connection.unregister_active_turn(&thread_id).await;
            return Err(err);
        }

        let summary = loop {
            tokio::select! {
                maybe_event = event_rx.recv() => {
                    match maybe_event {
                        Some(AppServerTurnEvent::Forward(event)) => on_event(event).await?,
                        Some(AppServerTurnEvent::Completed(result)) => {
                            let result = result?;
                            break CodexTurnSummary { thread_id: Some(result.thread_id) };
                        }
                        None => bail!("codex app-server closed the active turn event channel"),
                    }
                }
                completion = &mut completion_rx => {
                    let result = completion
                        .map_err(|_| anyhow!("codex app-server completion channel closed"))??;
                    break CodexTurnSummary { thread_id: Some(result.thread_id) };
                }
            }
        };

        connection.unregister_active_turn(&thread_id).await;
        Ok(summary)
    }

    pub async fn respond_to_pending(
        &self,
        token: &str,
        action: PendingInteractionAction,
    ) -> Result<()> {
        let connection = self.ensure_connection().await?;
        connection.respond_to_pending(token, action).await
    }

    pub async fn list_pending_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingInteractionSummary>> {
        let connection = self.ensure_connection().await?;
        Ok(connection.list_pending_for_thread(thread_id).await)
    }

    async fn ensure_connection(&self) -> Result<Arc<AppServerConnection>> {
        if let Some(existing) = self.connection.lock().await.as_ref().cloned() {
            return Ok(existing);
        }

        let connection = AppServerConnection::spawn(self.config.clone()).await?;
        let mut slot = self.connection.lock().await;
        if let Some(existing) = slot.as_ref().cloned() {
            Ok(existing)
        } else {
            *slot = Some(connection.clone());
            Ok(connection)
        }
    }
}

struct AppServerConnection {
    config: CodexConfig,
    outbound_tx: mpsc::UnboundedSender<WsMessage>,
    pending_requests: Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>,
    active_turns: Mutex<HashMap<String, ActiveTurnHandle>>,
    pending_interactions: Mutex<HashMap<String, PendingInteraction>>,
    next_request_id: AtomicU64,
    next_interaction_id: AtomicU64,
    _child: Mutex<Child>,
}

struct ActiveTurnHandle {
    event_tx: mpsc::UnboundedSender<AppServerTurnEvent>,
    completion_tx: Option<oneshot::Sender<Result<AppServerTurnResult>>>,
}

#[derive(Clone)]
struct PendingInteraction {
    sequence: u64,
    token: String,
    rpc_id: Value,
    thread_id: String,
    kind: PendingInteractionKind,
}

#[derive(Debug)]
enum AppServerTurnEvent {
    Forward(CodexStreamEvent),
    Completed(Result<AppServerTurnResult>),
}

#[derive(Debug)]
struct AppServerTurnResult {
    thread_id: String,
}

impl AppServerConnection {
    async fn spawn(config: CodexConfig) -> Result<Arc<Self>> {
        let mut child = build_app_server_command(&config)?;
        child.kill_on_drop(true);
        let mut child = child
            .spawn()
            .context("failed to spawn codex app-server process")?;

        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim();
                    if !line.is_empty() {
                        debug!("codex app-server stdout: {line}");
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim();
                    if !line.is_empty() {
                        warn!("codex app-server stderr: {line}");
                    }
                }
            });
        }

        let socket = connect_with_retry(&config.app_server_url).await?;
        let (mut writer, mut reader) = socket.split();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        let connection = Arc::new(Self {
            config,
            outbound_tx,
            pending_requests: Mutex::new(HashMap::new()),
            active_turns: Mutex::new(HashMap::new()),
            pending_interactions: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            next_interaction_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        });

        let writer_connection = connection.clone();
        tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                if let Err(err) = writer.send(message).await {
                    warn!("codex app-server write failed: {err}");
                    writer_connection
                        .fail_all_active_turns("codex app-server websocket write failed")
                        .await;
                    break;
                }
            }
        });

        let reader_connection = connection.clone();
        tokio::spawn(async move {
            loop {
                match reader.next().await {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Err(err) = reader_connection.handle_incoming_message(&text).await {
                            warn!("codex app-server message handling failed: {err:#}");
                        }
                    }
                    Some(Ok(WsMessage::Binary(payload))) => {
                        let text = String::from_utf8_lossy(&payload);
                        if let Err(err) = reader_connection.handle_incoming_message(&text).await {
                            warn!("codex app-server binary message handling failed: {err:#}");
                        }
                    }
                    Some(Ok(WsMessage::Close(frame))) => {
                        warn!("codex app-server websocket closed: {:?}", frame);
                        reader_connection
                            .fail_all_active_turns("codex app-server websocket closed")
                            .await;
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        warn!("codex app-server websocket read failed: {err}");
                        reader_connection
                            .fail_all_active_turns("codex app-server websocket read failed")
                            .await;
                        break;
                    }
                    None => {
                        reader_connection
                            .fail_all_active_turns("codex app-server websocket ended")
                            .await;
                        break;
                    }
                }
            }
        });

        connection.initialize().await?;
        Ok(connection)
    }

    async fn initialize(&self) -> Result<()> {
        let params = json!({
            "clientInfo": {
                "name": env!("CARGO_PKG_NAME"),
                "title": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": true
            }
        });

        timeout(
            Duration::from_secs(APP_SERVER_INIT_TIMEOUT_SECS),
            self.call("initialize", params),
        )
        .await
        .context("timed out while initializing codex app-server")??;
        Ok(())
    }

    async fn start_thread(&self, working_directory: &std::path::Path) -> Result<String> {
        let params = json!({
            "cwd": working_directory.display().to_string(),
            "approvalPolicy": self.json_approval_policy_value(),
            "approvalsReviewer": "user",
            "sandbox": self.json_sandbox_mode_value(),
            "experimentalRawEvents": false,
            "persistExtendedHistory": true,
        });

        let result = self.call("thread/start", params).await?;
        extract_thread_id(&result)
    }

    async fn resume_thread(
        &self,
        thread_id: &str,
        working_directory: &std::path::Path,
    ) -> Result<String> {
        let params = json!({
            "threadId": thread_id,
            "cwd": working_directory.display().to_string(),
            "approvalPolicy": self.json_approval_policy_value(),
            "approvalsReviewer": "user",
            "sandbox": self.json_sandbox_mode_value(),
            "persistExtendedHistory": true,
        });

        let result = self.call("thread/resume", params).await?;
        extract_thread_id(&result)
    }

    async fn start_turn(
        &self,
        thread_id: &str,
        prompt: &str,
        collaboration_mode: Option<&CollaborationModePreset>,
    ) -> Result<()> {
        let mut params = json!({
            "threadId": thread_id,
            "input": [
                {
                    "type": "text",
                    "text": prompt,
                    "text_elements": []
                }
            ],
            "approvalPolicy": self.json_approval_policy_value(),
            "approvalsReviewer": "user",
        });

        if let Some(mode) = collaboration_mode {
            params["collaborationMode"] = self.json_collaboration_mode_value(mode);
        }

        let _ = self.call("turn/start", params).await?;
        Ok(())
    }

    async fn register_active_turn(
        &self,
        thread_id: String,
        event_tx: mpsc::UnboundedSender<AppServerTurnEvent>,
    ) -> Result<oneshot::Receiver<Result<AppServerTurnResult>>> {
        let (completion_tx, completion_rx) = oneshot::channel();
        let mut active_turns = self.active_turns.lock().await;
        if active_turns.contains_key(&thread_id) {
            bail!("codex already has an active turn for thread `{thread_id}`");
        }
        active_turns.insert(
            thread_id,
            ActiveTurnHandle {
                event_tx,
                completion_tx: Some(completion_tx),
            },
        );
        Ok(completion_rx)
    }

    async fn unregister_active_turn(&self, thread_id: &str) {
        self.active_turns.lock().await.remove(thread_id);
    }

    async fn list_pending_for_thread(&self, thread_id: &str) -> Vec<PendingInteractionSummary> {
        let mut pending = self
            .pending_interactions
            .lock()
            .await
            .values()
            .filter(|interaction| interaction.thread_id == thread_id)
            .cloned()
            .collect::<Vec<_>>();
        pending.sort_by_key(|interaction| interaction.sequence);
        pending
            .into_iter()
            .map(|interaction| interaction.kind.summary(interaction.token.clone()))
            .collect()
    }

    async fn respond_to_pending(
        &self,
        token: &str,
        action: PendingInteractionAction,
    ) -> Result<()> {
        let interaction = {
            let pending = self.pending_interactions.lock().await;
            pending
                .get(token)
                .cloned()
                .ok_or_else(|| anyhow!("no pending codex interaction found for token `{token}`"))?
        };

        let result = build_interaction_response(&interaction.kind, action)?;
        self.send_json(json!({
            "id": interaction.rpc_id,
            "result": result,
        }))
        .await?;

        self.pending_interactions.lock().await.remove(token);
        Ok(())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = format!(
            "codex-channel-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.pending_requests.lock().await.insert(id.clone(), tx);
        self.send_json(json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        timeout(Duration::from_secs(APP_SERVER_RPC_TIMEOUT_SECS), rx)
            .await
            .with_context(|| format!("timed out waiting for codex app-server response to `{method}`"))?
            .map_err(|_| anyhow!("codex app-server response channel closed for `{method}`"))?
    }

    async fn send_json(&self, value: Value) -> Result<()> {
        self.outbound_tx
            .send(WsMessage::Text(value.to_string()))
            .map_err(|_| anyhow!("codex app-server send channel is closed"))
    }

    async fn handle_incoming_message(&self, text: &str) -> Result<()> {
        let value: Value = serde_json::from_str(text).context("invalid json from codex app-server")?;
        let Some(object) = value.as_object() else {
            return Ok(());
        };

        if object.contains_key("method") && object.contains_key("id") {
            self.handle_server_request(object).await
        } else if object.contains_key("method") {
            self.handle_notification(object).await
        } else if object.contains_key("id") {
            self.handle_response(object).await
        } else {
            Ok(())
        }
    }

    async fn handle_response(&self, object: &Map<String, Value>) -> Result<()> {
        let id = stringify_id(
            object
                .get("id")
                .ok_or_else(|| anyhow!("missing response id from codex app-server"))?,
        )?;
        let tx = self.pending_requests.lock().await.remove(&id);
        let Some(tx) = tx else {
            return Ok(());
        };

        if let Some(error) = object.get("error") {
            let _ = tx.send(Err(anyhow!("codex app-server returned error: {error}")));
            return Ok(());
        }

        let result = object.get("result").cloned().unwrap_or(Value::Null);
        let _ = tx.send(Ok(result));
        Ok(())
    }

    async fn handle_notification(&self, object: &Map<String, Value>) -> Result<()> {
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing notification method from codex app-server"))?;
        let params = object.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "item/agentMessage/delta" => {
                let thread_id = required_string(&params, "threadId")?;
                let delta = required_string(&params, "delta")?;
                self.forward_to_turn(
                    &thread_id,
                    AppServerTurnEvent::Forward(CodexStreamEvent::AgentMessage {
                        text: delta,
                        is_partial: true,
                    }),
                )
                .await;
            }
            "item/completed" => {
                let thread_id = required_string(&params, "threadId")?;
                let item = params.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                    let text = required_string(&item, "text")?;
                    self.forward_to_turn(
                        &thread_id,
                        AppServerTurnEvent::Forward(CodexStreamEvent::AgentMessage {
                            text,
                            is_partial: false,
                        }),
                    )
                    .await;
                }
            }
            "error" => {
                let thread_id = required_string(&params, "threadId").ok();
                let message = params
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("codex app-server reported an error")
                    .to_owned();

                if let Some(thread_id) = thread_id {
                    self.forward_to_turn(
                        &thread_id,
                        AppServerTurnEvent::Forward(CodexStreamEvent::Notice(message)),
                    )
                    .await;
                }
            }
            "turn/completed" => {
                let thread_id = required_string(&params, "threadId")?;
                let turn = params.get("turn").cloned().unwrap_or(Value::Null);
                let status = turn
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                if status == "completed" {
                    self.complete_turn(&thread_id, Ok(AppServerTurnResult {
                        thread_id: thread_id.clone(),
                    }))
                        .await;
                } else {
                    let message = turn
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("codex turn failed")
                        .to_owned();
                    self.complete_turn(&thread_id, Err(anyhow!(message))).await;
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn handle_server_request(&self, object: &Map<String, Value>) -> Result<()> {
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing server request method from codex app-server"))?;
        let rpc_id = clone_request_id(
            object
                .get("id")
                .ok_or_else(|| anyhow!("missing server request id from codex app-server"))?,
        )?;
        let params = object.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "item/commandExecution/requestApproval" => {
                let thread_id = required_string(&params, "threadId")?;
                let (sequence, token) = self.next_interaction_token();
                let kind = PendingInteractionKind::CommandApproval {
                    command: optional_string(&params, "command"),
                    cwd: optional_string(&params, "cwd"),
                    reason: optional_string(&params, "reason"),
                };
                self.store_pending_and_forward(sequence, token, rpc_id, thread_id, kind)
                    .await;
            }
            "item/fileChange/requestApproval" => {
                let thread_id = required_string(&params, "threadId")?;
                let (sequence, token) = self.next_interaction_token();
                let kind = PendingInteractionKind::FileChangeApproval;
                self.store_pending_and_forward(sequence, token, rpc_id, thread_id, kind)
                    .await;
            }
            "item/permissions/requestApproval" => {
                let thread_id = required_string(&params, "threadId")?;
                let (sequence, token) = self.next_interaction_token();
                let kind = PendingInteractionKind::PermissionsApproval {
                    permissions: params.get("permissions").cloned().unwrap_or(Value::Null),
                    reason: optional_string(&params, "reason"),
                };
                self.store_pending_and_forward(sequence, token, rpc_id, thread_id, kind)
                    .await;
            }
            "item/tool/requestUserInput" => {
                let thread_id = required_string(&params, "threadId")?;
                let (sequence, token) = self.next_interaction_token();
                let questions = params
                    .get("questions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let kind = PendingInteractionKind::UserInputRequest { questions };
                self.store_pending_and_forward(sequence, token, rpc_id, thread_id, kind)
                    .await;
            }
            _ => {
                debug!("ignoring unsupported codex app-server request method `{method}`");
            }
        }

        Ok(())
    }

    async fn store_pending_and_forward(
        &self,
        sequence: u64,
        token: String,
        rpc_id: Value,
        thread_id: String,
        kind: PendingInteractionKind,
    ) {
        self.pending_interactions.lock().await.insert(
            token.clone(),
            PendingInteraction {
                sequence,
                token: token.clone(),
                rpc_id,
                thread_id: thread_id.clone(),
                kind: kind.clone(),
            },
        );
        self.forward_to_turn(
            &thread_id,
            AppServerTurnEvent::Forward(CodexStreamEvent::PendingInteraction(
                kind.summary(token),
            )),
        )
        .await;
    }

    async fn forward_to_turn(&self, thread_id: &str, event: AppServerTurnEvent) {
        if let Some(handle) = self.active_turns.lock().await.get(thread_id) {
            let _ = handle.event_tx.send(event);
        }
    }

    async fn complete_turn(&self, thread_id: &str, result: Result<AppServerTurnResult>) {
        if let Some(handle) = self.active_turns.lock().await.get_mut(thread_id) {
            let forwarded = result
                .as_ref()
                .map(|value| AppServerTurnResult {
                    thread_id: value.thread_id.clone(),
                })
                .map_err(|err| anyhow!(err.to_string()));
            let _ = handle
                .event_tx
                .send(AppServerTurnEvent::Completed(forwarded));
            if let Some(completion_tx) = handle.completion_tx.take() {
                let _ = completion_tx.send(result);
            }
        }
    }

    async fn fail_all_active_turns(&self, message: &str) {
        let mut active_turns = self.active_turns.lock().await;
        for (_thread_id, handle) in active_turns.iter_mut() {
            if let Some(completion_tx) = handle.completion_tx.take() {
                let _ = completion_tx.send(Err(anyhow!(message.to_owned())));
            }
        }
        active_turns.clear();
    }

    fn next_interaction_token(&self) -> (u64, String) {
        let sequence = self.next_interaction_id.fetch_add(1, Ordering::Relaxed);
        (sequence, format!("req-{sequence}"))
    }

    fn json_approval_policy_value(&self) -> Value {
        match self.config.ask_for_approval.as_deref() {
            Some("untrusted") => json!("untrusted"),
            Some("on-request") => json!("on-request"),
            Some("on-failure") => json!("on-failure"),
            Some("never") => json!("never"),
            _ => json!("on-request"),
        }
    }

    fn json_sandbox_mode_value(&self) -> Value {
        match self.config.sandbox.as_deref() {
            Some("read-only") => json!("read-only"),
            Some("danger-full-access") => json!("danger-full-access"),
            Some("workspace-write") => json!("workspace-write"),
            _ => json!("workspace-write"),
        }
    }

    fn json_collaboration_mode_value(&self, mode: &CollaborationModePreset) -> Value {
        let model = self
            .config
            .model
            .clone()
            .unwrap_or_else(|| "gpt-5.4".to_owned());

        json!({
            "mode": mode.as_codex_mode(),
            "settings": {
                "model": model,
                "reasoning_effort": Value::Null,
                "developer_instructions": Value::Null,
            }
        })
    }
}

fn build_app_server_command(config: &CodexConfig) -> Result<Command> {
    let program = config
        .launcher
        .first()
        .ok_or_else(|| anyhow!("codex launcher cannot be empty"))?;

    let mut command = Command::new(program);
    for arg in config.launcher.iter().skip(1) {
        command.arg(arg);
    }

    command.current_dir(&config.working_directory);
    apply_proxy_environment(&mut command, config);
    command.arg("app-server");
    command.arg("--listen");
    command.arg(&config.app_server_url);
    command.arg("--session-source");
    command.arg(APP_SERVER_SESSION_SOURCE);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    Ok(command)
}

fn apply_proxy_environment(command: &mut Command, config: &CodexConfig) {
    apply_optional_env(command, "HTTP_PROXY", config.http_proxy.as_deref());
    apply_optional_env(command, "HTTPS_PROXY", config.https_proxy.as_deref());
    apply_optional_env(command, "ALL_PROXY", config.all_proxy.as_deref());
    apply_optional_env(command, "NO_PROXY", config.no_proxy.as_deref());
    apply_optional_env(command, "http_proxy", config.http_proxy.as_deref());
    apply_optional_env(command, "https_proxy", config.https_proxy.as_deref());
    apply_optional_env(command, "all_proxy", config.all_proxy.as_deref());
    apply_optional_env(command, "no_proxy", config.no_proxy.as_deref());
}

async fn connect_with_retry(
    url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
> {
    let mut last_error = None;
    for _ in 0..25 {
        match connect_async(url).await {
            Ok((socket, _)) => return Ok(socket),
            Err(err) => {
                last_error = Some(err);
                sleep(Duration::from_millis(200)).await;
            }
        }
    }

    Err(anyhow!(
        "failed to connect to codex app-server at `{url}`: {}",
        last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "unknown error".to_owned())
    ))
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("missing string field `{key}` in codex app-server payload"))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn stringify_id(value: &Value) -> Result<String> {
    if let Some(value) = value.as_str() {
        return Ok(value.to_owned());
    }
    if let Some(value) = value.as_i64() {
        return Ok(value.to_string());
    }
    bail!("unsupported json-rpc id from codex app-server: {value}")
}

fn clone_request_id(value: &Value) -> Result<Value> {
    if value.is_string() || value.is_number() {
        return Ok(value.clone());
    }
    bail!("unsupported json-rpc id from codex app-server: {value}")
}

fn extract_thread_id(value: &Value) -> Result<String> {
    value
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("codex app-server response missing thread id"))
}

fn granted_permissions_value(requested: Value) -> Value {
    if requested.is_object() {
        requested
    } else {
        json!({})
    }
}

fn build_interaction_response(
    kind: &PendingInteractionKind,
    action: PendingInteractionAction,
) -> Result<Value> {
    match kind {
        PendingInteractionKind::CommandApproval { .. } => match action {
            PendingInteractionAction::Approve => Ok(json!({ "decision": "accept" })),
            PendingInteractionAction::ApproveForSession => {
                Ok(json!({ "decision": "acceptForSession" }))
            }
            PendingInteractionAction::Deny => Ok(json!({ "decision": "decline" })),
            PendingInteractionAction::Cancel => Ok(json!({ "decision": "cancel" })),
            PendingInteractionAction::ReplyText(_) | PendingInteractionAction::ReplyJson(_) => {
                bail!("this codex request expects approval, not free-form input")
            }
        },
        PendingInteractionKind::FileChangeApproval => match action {
            PendingInteractionAction::Approve => Ok(json!({ "decision": "accept" })),
            PendingInteractionAction::ApproveForSession => {
                Ok(json!({ "decision": "acceptForSession" }))
            }
            PendingInteractionAction::Deny => Ok(json!({ "decision": "decline" })),
            PendingInteractionAction::Cancel => Ok(json!({ "decision": "cancel" })),
            PendingInteractionAction::ReplyText(_) | PendingInteractionAction::ReplyJson(_) => {
                bail!("this codex request expects approval, not free-form input")
            }
        },
        PendingInteractionKind::PermissionsApproval { permissions, .. } => match action {
            PendingInteractionAction::Approve => Ok(json!({
                "permissions": granted_permissions_value(permissions.clone()),
                "scope": "turn",
            })),
            PendingInteractionAction::ApproveForSession => Ok(json!({
                "permissions": granted_permissions_value(permissions.clone()),
                "scope": "session",
            })),
            PendingInteractionAction::Deny | PendingInteractionAction::Cancel => Ok(json!({
                "permissions": {},
                "scope": "turn",
            })),
            PendingInteractionAction::ReplyText(_) | PendingInteractionAction::ReplyJson(_) => {
                bail!("this codex request expects permission approval, not free-form input")
            }
        },
        PendingInteractionKind::UserInputRequest { questions } => match action {
            PendingInteractionAction::ReplyText(text) => {
                if questions.len() != 1 {
                    bail!("this codex request expects answers for multiple questions; use JSON with `/reply <token> {{...}}`");
                }
                let question_id = questions
                    .first()
                    .and_then(|question| question.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("codex user-input request is missing a question id"))?;
                Ok(json!({
                    "answers": {
                        question_id: {
                            "answers": [text]
                        }
                    }
                }))
            }
            PendingInteractionAction::ReplyJson(value) => {
                let answers = value
                    .as_object()
                    .ok_or_else(|| anyhow!("expected a JSON object mapping question ids to answers"))?;
                let mut mapped = Map::new();
                for (question_id, answer_value) in answers {
                    let answer_list = if let Some(text) = answer_value.as_str() {
                        vec![Value::String(text.to_owned())]
                    } else if let Some(values) = answer_value.as_array() {
                        values
                            .iter()
                            .map(|value| {
                                value.as_str()
                                    .map(|text| Value::String(text.to_owned()))
                                    .ok_or_else(|| anyhow!("all answer values must be strings"))
                            })
                            .collect::<Result<Vec<_>>>()?
                    } else {
                        bail!("answer for question `{question_id}` must be a string or string array");
                    };

                    mapped.insert(
                        question_id.clone(),
                        json!({
                            "answers": answer_list
                        }),
                    );
                }
                Ok(Value::Object(
                    [("answers".to_owned(), Value::Object(mapped))]
                        .into_iter()
                        .collect(),
                ))
            }
            PendingInteractionAction::Approve
            | PendingInteractionAction::ApproveForSession
            | PendingInteractionAction::Deny
            | PendingInteractionAction::Cancel => {
                bail!("this codex request expects user input, not an approval decision")
            }
        },
    }
}
