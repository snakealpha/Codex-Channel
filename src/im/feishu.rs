use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use reqwest::Client;
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::{interval, sleep};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::{client_async_tls, connect_async};
use tracing::{debug, error, info, warn};

use crate::config::FeishuAdapterConfig;
use crate::im::ImAdapter;
use crate::model::{
    InboundMessage, OutboundMessage, OutboundMessageKind, PendingInteractionKind,
    PendingInteractionSummary,
};

const FEISHU_WS_ENDPOINT_PATH: &str = "/callback/ws/endpoint";
const FEISHU_TENANT_TOKEN_PATH: &str = "/open-apis/auth/v3/tenant_access_token/internal";
const FEISHU_SEND_MESSAGE_PATH: &str = "/open-apis/im/v1/messages?receive_id_type=chat_id";
const FEISHU_UPDATE_MESSAGE_PATH_PREFIX: &str = "/open-apis/im/v1/messages";
const HEADER_TYPE: &str = "type";
const HEADER_BIZ_RT: &str = "biz_rt";
const MESSAGE_TYPE_EVENT: &str = "event";
const MESSAGE_TYPE_PING: &str = "ping";
const MESSAGE_TYPE_PONG: &str = "pong";
const FRAME_TYPE_CONTROL: i32 = 0;
const FRAME_TYPE_DATA: i32 = 1;

#[derive(Clone)]
pub struct FeishuAdapter {
    app_id: String,
    app_secret: String,
    base_url: String,
    reconnect_interval: Duration,
    http: Client,
    outbound_tx: Arc<Mutex<Option<mpsc::Sender<OutboundMessage>>>>,
    tenant_token: Arc<RwLock<Option<CachedTenantToken>>>,
    conversation_state: Arc<Mutex<HashMap<String, ConversationStreamState>>>,
    pending_card_messages: Arc<Mutex<HashMap<String, String>>>,
}

impl FeishuAdapter {
    pub fn new(config: FeishuAdapterConfig) -> Result<Self> {
        let app_id = resolve_secret_env("app_id", config.app_id)?;
        let app_secret = resolve_secret_env("app_secret", config.app_secret)?;

        Ok(Self {
            app_id,
            app_secret,
            base_url: config.base_url.trim_end_matches('/').to_owned(),
            reconnect_interval: Duration::from_secs(config.reconnect_interval_secs.max(1)),
            http: Client::builder()
                .user_agent(format!(
                    "{}/{}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                ))
                .build()
                .context("failed to build reqwest client")?,
            outbound_tx: Arc::new(Mutex::new(None)),
            tenant_token: Arc::new(RwLock::new(None)),
            conversation_state: Arc::new(Mutex::new(HashMap::new())),
            pending_card_messages: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn run_forever(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()> {
        loop {
            match self.run_connection(inbound.clone()).await {
                Ok(()) => {
                    warn!("feishu websocket connection closed, reconnecting soon");
                }
                Err(err) => {
                    error!("feishu adapter connection failed: {err:#}");
                }
            }
            sleep(self.reconnect_interval).await;
        }
    }

    async fn run_connection(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()> {
        let endpoint = self.fetch_ws_endpoint().await?;
        let service_id = extract_service_id(&endpoint.url)
            .ok_or_else(|| anyhow!("feishu websocket url missing service_id query parameter"))?;

        info!("connecting to feishu long connection websocket");
        let socket = connect_feishu_websocket(&endpoint.url)
            .await
            .context("failed to connect to feishu websocket")?;
        let (mut writer, mut reader) = socket.split();

        let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundMessage>(64);
        {
            let mut slot = self.outbound_tx.lock().await;
            *slot = Some(outbound_tx);
        }

        let mut ping_interval = interval(Duration::from_secs(
            endpoint.client_config.ping_interval.max(1) as u64,
        ));

        loop {
            tokio::select! {
                message = reader.next() => {
                    match message {
                        Some(Ok(WsMessage::Binary(payload))) => {
                            if let Some(reply_frame) = self.handle_incoming_frame(payload, &inbound).await? {
                                writer.send(WsMessage::Binary(reply_frame)).await
                                    .context("failed to send frame back to feishu")?;
                            }
                        }
                        Some(Ok(WsMessage::Ping(payload))) => {
                            writer.send(WsMessage::Pong(payload)).await
                                .context("failed to send websocket pong to feishu")?;
                        }
                        Some(Ok(WsMessage::Close(frame))) => {
                            warn!("feishu websocket closed by server: {:?}", frame);
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(err)) => return Err(err).context("failed to read feishu websocket message"),
                        None => break,
                    }
                }
                outbound = outbound_rx.recv() => {
                    match outbound {
                        Some(message) => self.handle_outbound_message(message).await?,
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    let ping = build_ping_frame(service_id);
                    writer.send(WsMessage::Binary(ping)).await
                        .context("failed to send feishu ping frame")?;
                }
            }
        }

        {
            let mut slot = self.outbound_tx.lock().await;
            *slot = None;
        }

        Ok(())
    }

    async fn fetch_ws_endpoint(&self) -> Result<EndpointData> {
        let response = self
            .http
            .post(format!("{}{}", self.base_url, FEISHU_WS_ENDPOINT_PATH))
            .header("locale", "zh")
            .json(&serde_json::json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await
            .context("failed to request feishu websocket endpoint")?;

        let status = response.status();
        let payload: EndpointResponse = response
            .json()
            .await
            .context("failed to parse feishu websocket endpoint response")?;

        if !status.is_success() {
            bail!(
                "feishu websocket endpoint http status {status}: {:?}",
                payload.msg
            );
        }
        if payload.code != 0 {
            bail!(
                "feishu websocket endpoint returned code {}: {}",
                payload.code,
                payload.msg.unwrap_or_else(|| "unknown error".to_owned())
            );
        }

        payload
            .data
            .ok_or_else(|| anyhow!("feishu websocket endpoint response missing data"))
    }

    async fn handle_incoming_frame(
        &self,
        payload: Vec<u8>,
        inbound: &mpsc::Sender<InboundMessage>,
    ) -> Result<Option<Vec<u8>>> {
        let frame = FeishuFrame::decode(payload.as_slice())
            .context("failed to decode feishu protobuf frame")?;
        match frame.method {
            FRAME_TYPE_CONTROL => Ok(self.handle_control_frame(frame)?),
            FRAME_TYPE_DATA => self.handle_data_frame(frame, inbound).await,
            other => {
                warn!("ignoring unknown feishu frame type {other}");
                Ok(None)
            }
        }
    }

    fn handle_control_frame(&self, frame: FeishuFrame) -> Result<Option<Vec<u8>>> {
        let headers = FrameHeaders::new(frame.headers);
        match headers.get_string(HEADER_TYPE).as_deref() {
            Some(MESSAGE_TYPE_PONG) => Ok(None),
            Some(MESSAGE_TYPE_PING) => Ok(Some(build_pong_frame(frame.service))),
            Some(other) => {
                warn!("ignoring unsupported feishu control frame type {other}");
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn handle_data_frame(
        &self,
        frame: FeishuFrame,
        inbound: &mpsc::Sender<InboundMessage>,
    ) -> Result<Option<Vec<u8>>> {
        let start = Instant::now();
        let headers = FrameHeaders::new(frame.headers.clone());
        let message_type = headers.get_string(HEADER_TYPE).unwrap_or_default();

        if message_type != MESSAGE_TYPE_EVENT {
            return Ok(Some(build_response_frame(
                frame,
                FrameHeaders::from(headers)
                    .with(HEADER_BIZ_RT, start.elapsed().as_millis().to_string()),
                ResponseEnvelope::ok(),
            )?));
        }

        let event: EventEnvelope = serde_json::from_slice(&frame.payload)
            .context("failed to parse feishu event payload")?;

        let response = match event.header.event_type.as_str() {
            "im.message.receive_v1" => {
                if let Some(message) = event.event.message.as_ref() {
                    debug!(
                        feishu_message_type =
                            message.message_type.as_deref().unwrap_or("<missing>"),
                        chat_id = message.chat_id.as_deref().unwrap_or("<missing>"),
                        thread_id = message.thread_id.as_deref().unwrap_or("<missing>"),
                        raw_content = message.content.as_deref().unwrap_or("<missing>"),
                        "received raw feishu inbound message"
                    );
                    if let Some(text) = message.extract_text()? {
                        debug!(
                            feishu_message_type =
                                message.message_type.as_deref().unwrap_or("<missing>"),
                            extracted_text = %text,
                            "extracted textual content from feishu inbound message"
                        );
                        let conversation_id = message
                            .thread_id
                            .clone()
                            .or_else(|| message.chat_id.clone())
                            .ok_or_else(|| {
                                anyhow!("feishu event missing both thread_id and chat_id")
                            })?;

                        inbound
                            .send(InboundMessage {
                                adapter: self.name().to_owned(),
                                conversation_id,
                                sender_id: event
                                    .event
                                    .sender
                                    .as_ref()
                                    .and_then(|sender| sender.sender_id.as_ref())
                                    .and_then(|id| {
                                        id.open_id
                                            .clone()
                                            .or_else(|| id.user_id.clone())
                                            .or_else(|| id.union_id.clone())
                                    }),
                                text,
                            })
                            .await
                            .map_err(|_| {
                                anyhow!(
                                    "gateway stopped before feishu message could be delivered"
                                )
                            })?;
                    } else {
                        debug!(
                            message_type = message.message_type.as_deref().unwrap_or("<missing>"),
                            raw_content = message.content.as_deref().unwrap_or("<missing>"),
                            "ignoring feishu message because no textual content could be extracted"
                        );
                    }
                }
                ResponseEnvelope::ok()
            }
            "card.action.trigger" => {
                if let Some(action) = self.card_action_to_inbound(&event)? {
                    info!(
                        token = action.token.as_deref().unwrap_or("<missing>"),
                        selected_label = action.selected_label.as_deref().unwrap_or("<missing>"),
                        callback_message_id =
                            action.open_message_id.as_deref().unwrap_or("<missing>"),
                        "received feishu card action"
                    );
                    inbound.send(action.message).await.map_err(|_| {
                        anyhow!("gateway stopped before feishu card action could be delivered")
                    })?;
                    let stored_message_id = if let Some(token) = action.token.as_deref() {
                        let action_key = action.action_key.as_deref().unwrap_or(token);
                        if action.delete_after_action {
                            self.remove_pending_card_message(action_key).await
                        } else {
                            self.get_pending_card_message(action_key).await
                        }
                    } else {
                        None
                    };
                    debug!(
                        token = action.token.as_deref().unwrap_or("<missing>"),
                        stored_message_id = stored_message_id.as_deref().unwrap_or("<missing>"),
                        callback_message_id =
                            action.open_message_id.as_deref().unwrap_or("<missing>"),
                        "resolved feishu card action message ids"
                    );
                    if action.delete_after_action {
                        if let Some(message_id) = stored_message_id
                            .as_deref()
                            .or(action.open_message_id.as_deref())
                        {
                            if let Err(err) = self.delete_message(message_id).await {
                                warn!("failed to delete feishu interaction card after action: {err:#}");
                            }
                        } else {
                            warn!("feishu card action did not include any message id to delete");
                        }
                    } else {
                        debug!("keeping feishu interaction card in place after action");
                    }
                    ResponseEnvelope::with_success_toast("Sent to Codex")
                } else {
                    ResponseEnvelope::ok()
                }
            }
            _ => ResponseEnvelope::ok(),
        };

        let response_headers = FrameHeaders::from(headers)
            .with(HEADER_BIZ_RT, start.elapsed().as_millis().to_string());
        Ok(Some(build_response_frame(
            frame,
            response_headers,
            response,
        )?))
    }

    async fn handle_outbound_message(&self, message: OutboundMessage) -> Result<()> {
        match message.kind {
            OutboundMessageKind::Status => {
                info!(
                    conversation_id = message.conversation_id,
                    "sending feishu status message"
                );
                let message_id = self
                    .send_text_message(&message.conversation_id, &message.text)
                    .await?;
                let mut state = self.conversation_state.lock().await;
                state.insert(
                    message.conversation_id,
                    ConversationStreamState {
                        draft_message_id: Some(message_id),
                    },
                );
                Ok(())
            }
            OutboundMessageKind::Agent => {
                info!(
                    conversation_id = message.conversation_id,
                    is_partial = message.is_partial,
                    "upserting feishu agent message"
                );
                if message.is_partial {
                    Ok(())
                } else {
                    self.upsert_stream_message(&message.conversation_id, &message.text, false)
                        .await
                }
            }
            OutboundMessageKind::Notice => {
                let keep_open = !message.text.starts_with("Codex failed:");
                info!(
                    conversation_id = message.conversation_id,
                    keep_open, "upserting feishu notice message"
                );
                if looks_like_markdown_notice_post(&message.text) {
                    self.clear_stream_state(&message.conversation_id).await;
                    let _ = self
                        .send_notice_post(&message.conversation_id, &message.text)
                        .await?;
                } else {
                    self.upsert_stream_message(&message.conversation_id, &message.text, keep_open)
                        .await?;
                }
                Ok(())
            }
            OutboundMessageKind::CommandResult => {
                info!(
                    conversation_id = message.conversation_id,
                    "sending feishu command result message"
                );
                if let Some(token) = message.dismiss_pending_token.as_deref() {
                    for message_id in self.take_all_pending_card_messages_for_token(token).await {
                        if let Err(err) = self.delete_message(&message_id).await {
                            warn!(
                                "failed to delete feishu pending card after text command: {err:#}"
                            );
                        }
                    }
                }
                self.clear_stream_state(&message.conversation_id).await;
                let _ = self
                    .send_text_message(&message.conversation_id, &message.text)
                    .await?;
                Ok(())
            }
            OutboundMessageKind::PendingInteraction => {
                let summary = message.pending_interaction.as_ref().ok_or_else(|| {
                    anyhow!("missing pending interaction payload for feishu card")
                })?;
                info!(
                    conversation_id = message.conversation_id,
                    token = %summary.token,
                    "sending feishu pending interaction card"
                );
                self.upsert_stream_message(
                    &message.conversation_id,
                    pending_interaction_status_text(summary),
                    true,
                )
                .await?;
                let cards = self
                    .send_pending_interaction_cards(&message.conversation_id, summary)
                    .await?;
                for (action_key, message_id) in cards {
                    self.store_pending_card_message(action_key, message_id)
                        .await;
                }
                debug!(token = %summary.token, "stored feishu pending card message ids for later cleanup");
                Ok(())
            }
        }
    }

    fn card_action_to_inbound(&self, event: &EventEnvelope) -> Result<Option<CardActionInbound>> {
        let Some(action) = event.event.action.as_ref() else {
            return Ok(None);
        };

        let Some(value) = action.value.as_ref() else {
            return Ok(None);
        };

        let Some(kind) = value.get("kind").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        if kind != "codex_pending" {
            return Ok(None);
        }

        let conversation_id = value
            .get("conversation_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                event
                    .event
                    .context
                    .as_ref()
                    .and_then(|context| context.open_chat_id.clone())
            })
            .ok_or_else(|| anyhow!("feishu card action missing conversation id"))?;
        let text = value
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("feishu card action missing command"))?
            .to_owned();

        Ok(Some(CardActionInbound {
            message: InboundMessage {
                adapter: self.name().to_owned(),
                conversation_id,
                sender_id: event.event.operator.as_ref().and_then(|operator| {
                    Some(if !operator.open_id.is_empty() {
                        operator.open_id.clone()
                    } else {
                        operator
                            .user_id
                            .clone()
                            .or_else(|| operator.tenant_key.clone())?
                    })
                }),
                text,
            },
            selected_label: value
                .get("selected_label")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            token: value
                .get("token")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            action_key: value
                .get("action_key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            open_message_id: event
                .event
                .context
                .as_ref()
                .and_then(|context| context.open_message_id.clone()),
            delete_after_action: value
                .get("delete_after_action")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        }))
    }

    async fn upsert_stream_message(
        &self,
        conversation_id: &str,
        text: &str,
        keep_open: bool,
    ) -> Result<()> {
        let draft_message_id = {
            let state = self.conversation_state.lock().await;
            state
                .get(conversation_id)
                .and_then(|state| state.draft_message_id.clone())
        };

        if let Some(message_id) = draft_message_id {
            self.update_text_message(&message_id, text).await?;
        } else {
            let message_id = self.send_text_message(conversation_id, text).await?;
            let mut state = self.conversation_state.lock().await;
            state.insert(
                conversation_id.to_owned(),
                ConversationStreamState {
                    draft_message_id: Some(message_id),
                },
            );
        }

        if !keep_open {
            self.clear_stream_state(conversation_id).await;
        }

        Ok(())
    }

    async fn clear_stream_state(&self, conversation_id: &str) {
        let mut state = self.conversation_state.lock().await;
        state.remove(conversation_id);
    }

    async fn store_pending_card_message(&self, action_key: String, message_id: String) {
        self.pending_card_messages
            .lock()
            .await
            .insert(action_key, message_id);
    }

    async fn get_pending_card_message(&self, action_key: &str) -> Option<String> {
        self.pending_card_messages
            .lock()
            .await
            .get(action_key)
            .cloned()
    }

    async fn remove_pending_card_message(&self, action_key: &str) -> Option<String> {
        self.pending_card_messages.lock().await.remove(action_key)
    }

    async fn take_all_pending_card_messages_for_token(&self, token: &str) -> Vec<String> {
        let mut pending_cards = self.pending_card_messages.lock().await;
        let prefix = format!("{token}::");
        let keys = pending_cards
            .keys()
            .filter(|key| **key == token || key.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        let mut message_ids = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(message_id) = pending_cards.remove(&key) {
                message_ids.push(message_id);
            }
        }
        message_ids
    }

    async fn send_text_message(&self, conversation_id: &str, text: &str) -> Result<String> {
        info!(conversation_id, "sending feishu text message");
        let token = self.get_tenant_access_token().await?;
        let payload = serde_json::json!({
            "receive_id": conversation_id,
            "msg_type": "text",
            "content": serde_json::to_string(&serde_json::json!({ "text": text }))?,
        });

        let response = self
            .http
            .post(format!("{}{}", self.base_url, FEISHU_SEND_MESSAGE_PATH))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .context("failed to send feishu text message")?;

        let (status, body, raw_body) =
            parse_feishu_api_response::<CreateOrUpdateMessageData>(response, "send-message")
                .await?;

        if !status.is_success() || body.code != 0 {
            bail!(
                "feishu send-message failed with http {} and code {}: {} (body: {})",
                status,
                body.code,
                body.msg.unwrap_or_else(|| "unknown error".to_owned()),
                raw_body
            );
        }

        body.data
            .and_then(|data| data.message_id)
            .ok_or_else(|| anyhow!("feishu send-message response missing message_id"))
    }

    async fn send_pending_interaction_cards(
        &self,
        conversation_id: &str,
        summary: &PendingInteractionSummary,
    ) -> Result<Vec<(String, String)>> {
        let token = self.get_tenant_access_token().await?;
        let mut sent = Vec::new();

        for spec in build_pending_interaction_cards(conversation_id, summary) {
            let payload = serde_json::json!({
                "receive_id": conversation_id,
                "msg_type": "interactive",
                "content": serde_json::to_string(&spec.card)?,
            });

            let response = self
                .http
                .post(format!("{}{}", self.base_url, FEISHU_SEND_MESSAGE_PATH))
                .bearer_auth(&token)
                .json(&payload)
                .send()
                .await
                .context("failed to send feishu interactive card message")?;

            let (status, body, raw_body) =
                parse_feishu_api_response::<CreateOrUpdateMessageData>(response, "send-card")
                    .await?;

            if !status.is_success() || body.code != 0 {
                bail!(
                    "feishu send-card failed with http {} and code {}: {} (body: {})",
                    status,
                    body.code,
                    body.msg.unwrap_or_else(|| "unknown error".to_owned()),
                    raw_body
                );
            }

            let message_id = body
                .data
                .and_then(|data| data.message_id)
                .ok_or_else(|| anyhow!("feishu send-card response missing message_id"))?;
            sent.push((spec.action_key, message_id));
        }

        Ok(sent)
    }

    async fn send_notice_post(&self, conversation_id: &str, text: &str) -> Result<String> {
        info!(conversation_id, "sending feishu notice post");
        let token = self.get_tenant_access_token().await?;
        let payload = serde_json::json!({
            "receive_id": conversation_id,
            "msg_type": "post",
            "content": serde_json::to_string(&build_notice_post(text))?,
        });

        let response = self
            .http
            .post(format!("{}{}", self.base_url, FEISHU_SEND_MESSAGE_PATH))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .context("failed to send feishu notice post")?;

        let (status, body, raw_body) =
            parse_feishu_api_response::<CreateOrUpdateMessageData>(response, "send-notice-post")
                .await?;

        if !status.is_success() || body.code != 0 {
            bail!(
                "feishu send-notice-post failed with http {} and code {}: {} (body: {})",
                status,
                body.code,
                body.msg.unwrap_or_else(|| "unknown error".to_owned()),
                raw_body
            );
        }

        body.data
            .and_then(|data| data.message_id)
            .ok_or_else(|| anyhow!("feishu send-notice-card response missing message_id"))
    }

    async fn update_text_message(&self, message_id: &str, text: &str) -> Result<()> {
        info!(message_id, "updating feishu text message");
        let token = self.get_tenant_access_token().await?;
        let payload = serde_json::json!({
            "msg_type": "text",
            "content": serde_json::to_string(&serde_json::json!({ "text": text }))?,
        });

        let response = self
            .http
            .put(format!(
                "{}{}/{}",
                self.base_url, FEISHU_UPDATE_MESSAGE_PATH_PREFIX, message_id
            ))
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .context("failed to update feishu text message")?;

        let (status, body, raw_body) =
            parse_feishu_api_response::<CreateOrUpdateMessageData>(response, "update-message")
                .await?;

        if !status.is_success() || body.code != 0 {
            bail!(
                "feishu update-message failed with http {} and code {}: {} (body: {})",
                status,
                body.code,
                body.msg.unwrap_or_else(|| "unknown error".to_owned()),
                raw_body
            );
        }

        Ok(())
    }

    async fn delete_message(&self, message_id: &str) -> Result<()> {
        info!(message_id, "deleting feishu message");
        let token = self.get_tenant_access_token().await?;

        let response = self
            .http
            .delete(format!(
                "{}{}/{}",
                self.base_url, FEISHU_UPDATE_MESSAGE_PATH_PREFIX, message_id
            ))
            .bearer_auth(token)
            .send()
            .await
            .context("failed to delete feishu message")?;

        let (status, body, raw_body) =
            parse_feishu_api_response::<CreateOrUpdateMessageData>(response, "delete-message")
                .await?;

        if !status.is_success() || body.code != 0 {
            bail!(
                "feishu delete-message failed with http {} and code {}: {} (body: {})",
                status,
                body.code,
                body.msg.unwrap_or_else(|| "unknown error".to_owned()),
                raw_body
            );
        }

        Ok(())
    }

    async fn get_tenant_access_token(&self) -> Result<String> {
        {
            let cache = self.tenant_token.read().await;
            if let Some(cache) = cache.as_ref() {
                if cache.expires_at > Instant::now() {
                    return Ok(cache.token.clone());
                }
            }
        }

        let response = self
            .http
            .post(format!("{}{}", self.base_url, FEISHU_TENANT_TOKEN_PATH))
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .context("failed to request feishu tenant access token")?;

        let status = response.status();
        let body: TenantAccessTokenResponse = response
            .json()
            .await
            .context("failed to parse feishu tenant access token response")?;

        if !status.is_success() || body.code != 0 {
            bail!(
                "feishu tenant token request failed with http {} and code {}: {}",
                status,
                body.code,
                body.msg.unwrap_or_else(|| "unknown error".to_owned())
            );
        }

        let token = body
            .tenant_access_token
            .ok_or_else(|| anyhow!("feishu tenant token response missing tenant_access_token"))?;
        let expire = body.expire.unwrap_or(7200).max(120);

        let cache = CachedTenantToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expire.saturating_sub(60)),
        };
        {
            let mut slot = self.tenant_token.write().await;
            *slot = Some(cache);
        }
        Ok(token)
    }
}

#[async_trait]
impl ImAdapter for FeishuAdapter {
    fn name(&self) -> &'static str {
        "feishu"
    }

    async fn run(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()> {
        self.run_forever(inbound).await
    }

    async fn send(&self, message: OutboundMessage) -> Result<()> {
        let tx = {
            let slot = self.outbound_tx.lock().await;
            slot.clone()
        };

        let tx = tx.ok_or_else(|| anyhow!("feishu adapter is not connected yet"))?;
        tx.send(message)
            .await
            .map_err(|_| anyhow!("feishu outbound channel is closed"))
    }
}

async fn connect_feishu_websocket(
    endpoint_url: &str,
) -> Result<tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>> {
    if let Some(proxy_url) = resolve_proxy_for_ws_url(endpoint_url)? {
        info!(proxy = %proxy_url, "connecting to feishu websocket through proxy");
        return connect_websocket_via_http_proxy(endpoint_url, &proxy_url).await;
    }

    let (socket, _) = connect_async(endpoint_url).await?;
    Ok(socket)
}

async fn connect_websocket_via_http_proxy(
    endpoint_url: &str,
    proxy_url: &Url,
) -> Result<tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>> {
    if proxy_url.scheme() != "http" {
        bail!(
            "unsupported proxy scheme `{}` for feishu websocket; only http proxies are supported",
            proxy_url.scheme()
        );
    }
    if !proxy_url.username().is_empty() || proxy_url.password().is_some() {
        bail!("proxy authentication is not supported for feishu websocket yet");
    }

    let endpoint = Url::parse(endpoint_url)
        .with_context(|| format!("invalid feishu websocket url `{endpoint_url}`"))?;
    let host = endpoint
        .host_str()
        .ok_or_else(|| anyhow!("feishu websocket url missing host"))?;
    let port = endpoint
        .port_or_known_default()
        .ok_or_else(|| anyhow!("feishu websocket url missing port"))?;

    let proxy_host = proxy_url
        .host_str()
        .ok_or_else(|| anyhow!("proxy url missing host"))?;
    let proxy_port = proxy_url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("proxy url missing port"))?;

    let mut stream = TcpStream::connect((proxy_host, proxy_port))
        .await
        .with_context(|| format!("failed to connect to proxy {proxy_host}:{proxy_port}"))?;

    let connect_request = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    stream
        .write_all(connect_request.as_bytes())
        .await
        .context("failed to send CONNECT request to proxy")?;

    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .context("failed to read proxy CONNECT response")?;
        if read == 0 {
            bail!("proxy closed the connection before CONNECT completed");
        }
        response.extend_from_slice(&chunk[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if response.len() > 16 * 1024 {
            bail!("proxy CONNECT response headers are too large");
        }
    }

    let response_text = String::from_utf8_lossy(&response);
    if !response_text.starts_with("HTTP/1.1 200") && !response_text.starts_with("HTTP/1.0 200") {
        bail!(
            "proxy CONNECT failed: {}",
            response_text.lines().next().unwrap_or("unknown response")
        );
    }

    let (socket, _) = client_async_tls(endpoint_url, stream)
        .await
        .context("failed to complete websocket handshake through proxy")?;
    Ok(socket)
}

fn resolve_proxy_for_ws_url(endpoint_url: &str) -> Result<Option<Url>> {
    let endpoint = Url::parse(endpoint_url)
        .with_context(|| format!("invalid feishu websocket url `{endpoint_url}`"))?;
    let scheme = endpoint.scheme();

    let candidates = match scheme {
        "wss" => ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"].as_slice(),
        "ws" => ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"].as_slice(),
        _ => return Ok(None),
    };

    for key in candidates {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Url::parse(trimmed).map(Some).with_context(|| {
                    format!("invalid proxy url `{trimmed}` from env var `{key}`")
                });
            }
        }
    }

    Ok(None)
}

fn resolve_secret_env(field_name: &str, env_name: Option<String>) -> Result<String> {
    let env_name = env_name
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            anyhow!("missing feishu {field_name}; configure it with an environment variable name")
        })?;

    let value = std::env::var(&env_name)
        .with_context(|| format!("failed to read env var `{env_name}` for feishu {field_name}"))?;
    if !value.trim().is_empty() {
        return Ok(value);
    }

    bail!("env var `{env_name}` for feishu {field_name} is empty")
}

async fn parse_feishu_api_response<T>(
    response: reqwest::Response,
    operation: &str,
) -> Result<(reqwest::StatusCode, FeishuApiResponse<T>, String)>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let raw_body = response
        .text()
        .await
        .with_context(|| format!("failed to read feishu {operation} response body"))?;
    debug!(operation, status = %status, body = %raw_body, "received feishu api response");
    let body = serde_json::from_str::<FeishuApiResponse<T>>(&raw_body).with_context(|| {
        format!("failed to parse feishu {operation} response body as json: {raw_body}")
    })?;
    Ok((status, body, raw_body))
}

fn pending_interaction_status_text(summary: &PendingInteractionSummary) -> &'static str {
    match &summary.kind {
        PendingInteractionKind::CommandApproval { .. }
        | PendingInteractionKind::FileChangeApproval
        | PendingInteractionKind::PermissionsApproval { .. } => {
            "Codex is waiting for your approval in the card below..."
        }
        PendingInteractionKind::UserInputRequest { .. } => {
            "Codex is waiting for your reply in the card below..."
        }
    }
}

fn build_pending_interaction_cards(
    conversation_id: &str,
    summary: &PendingInteractionSummary,
) -> Vec<PendingCardSpec> {
    if let PendingInteractionKind::UserInputRequest { questions } = &summary.kind {
        if questions.len() > 1 {
            let mut cards = Vec::new();
            for (index, question) in questions.iter().enumerate() {
                let Some(question_id) = question.get("id").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                cards.push(PendingCardSpec {
                    action_key: pending_card_action_key(&summary.token, Some(question_id)),
                    card: build_multi_question_card(
                        conversation_id,
                        &summary.token,
                        question,
                        index + 1,
                        questions.len(),
                    ),
                });
            }
            if !cards.is_empty() {
                return cards;
            }
        }
    }

    vec![PendingCardSpec {
        action_key: pending_card_action_key(&summary.token, None),
        card: build_pending_interaction_card(conversation_id, summary),
    }]
}

fn build_pending_interaction_card(
    conversation_id: &str,
    summary: &PendingInteractionSummary,
) -> serde_json::Value {
    let (title, template) = pending_card_header_v2(summary);
    let markdown = pending_card_markdown_v2(summary);
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": markdown,
    })];

    for actions in pending_card_action_sections_v2(conversation_id, summary) {
        if !actions.is_empty() {
            elements.push(serde_json::json!({
                "tag": "action",
                "layout": "flow",
                "actions": actions,
            }));
        }
    }

    serde_json::json!({
        "config": {
            "wide_screen_mode": true,
            "enable_forward": true,
            "update_multi": true,
        },
        "header": {
            "title": {
                "tag": "plain_text",
                "content": title,
            },
            "template": template,
        },
        "elements": elements,
    })
}

fn build_notice_post(text: &str) -> serde_json::Value {
    serde_json::json!({
        "zh_cn": {
            "title": "",
            "content": build_notice_post_content_rows(text),
        }
    })
}

fn build_notice_post_content_rows(text: &str) -> Vec<serde_json::Value> {
    parse_notice_blocks(text)
        .into_iter()
        .filter_map(|block| match block {
            NoticeBlock::Text(text) => {
                let text = text.trim();
                if text.is_empty() {
                    None
                } else {
                    Some(serde_json::json!([
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
                    Some(serde_json::json!([
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

fn build_notice_card(text: &str) -> serde_json::Value {
    serde_json::json!({
        "config": {
            "wide_screen_mode": true,
            "enable_forward": true,
            "update_multi": true,
        },
        "header": {
            "title": {
                "tag": "plain_text",
                "content": "Codex Notice",
            },
            "template": "blue",
        },
        "elements": [
            {
                "tag": "markdown",
                "content": text,
            }
        ],
    })
}

fn build_multi_question_card(
    conversation_id: &str,
    token: &str,
    question: &serde_json::Value,
    question_index: usize,
    total_questions: usize,
) -> serde_json::Value {
    let header = question
        .get("header")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Question");
    let body = question
        .get("question")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("(missing question text)");
    let question_id = question
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("question_id");

    let mut lines = vec![
        format!("**Question {question_index}/{total_questions}: {header}**"),
        body.to_owned(),
    ];
    if let Some(options) = question
        .get("options")
        .and_then(serde_json::Value::as_array)
    {
        lines.push(String::new());
        let coded_options = render_coded_options(options);
        if !coded_options.is_empty() {
            lines.push("```text".to_owned());
            lines.extend(coded_options);
            lines.push("```".to_owned());
        }
        lines.push(String::new());
        lines
            .push("Click a coded option below, or use `Other` to type a custom answer.".to_owned());
    } else {
        lines.push(String::new());
        lines.push("Reply like this:".to_owned());
        lines.push("```text".to_owned());
        lines.push(format!("/reply {question_id} your answer"));
        lines.push("```".to_owned());
        lines.push("If there are multiple pending requests, use:".to_owned());
        lines.push("```text".to_owned());
        lines.push("/reply <item-number> <question_id> your answer".to_owned());
        lines.push("```".to_owned());
    }

    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": lines.join("\n"),
    })];

    for actions in multi_question_action_sections(conversation_id, token, question) {
        if !actions.is_empty() {
            elements.push(serde_json::json!({
                "tag": "action",
                "layout": "flow",
                "actions": actions,
            }));
        }
    }

    serde_json::json!({
        "config": {
            "wide_screen_mode": true,
            "enable_forward": true,
            "update_multi": true,
        },
        "header": {
            "title": {
                "tag": "plain_text",
                "content": format!("Codex Question {question_index}/{total_questions}"),
            },
            "template": "blue",
        },
        "elements": elements,
    })
}

fn pending_card_header_v2(summary: &PendingInteractionSummary) -> (&'static str, &'static str) {
    match &summary.kind {
        PendingInteractionKind::CommandApproval { .. } => ("Codex Command Approval", "orange"),
        PendingInteractionKind::FileChangeApproval => ("Codex File Change Approval", "red"),
        PendingInteractionKind::PermissionsApproval { .. } => {
            ("Codex Permissions Approval", "orange")
        }
        PendingInteractionKind::UserInputRequest { .. } => ("Codex Needs Your Choice", "blue"),
    }
}

fn pending_card_markdown_v2(summary: &PendingInteractionSummary) -> String {
    match &summary.kind {
        PendingInteractionKind::CommandApproval {
            command, reason, ..
        } => {
            let mut lines = vec![
                "Codex wants to run this command:".to_owned(),
                "```bash".to_owned(),
                command
                    .as_deref()
                    .unwrap_or("(command not provided)")
                    .to_owned(),
                "```".to_owned(),
            ];
            if let Some(reason) = reason.as_deref().filter(|text| !text.trim().is_empty()) {
                lines.push(String::new());
                lines.push(reason.to_owned());
            }
            lines.push(String::new());
            lines.push("If this is the only pending item, reply with:".to_owned());
            lines.push("```text".to_owned());
            lines.push("/approve".to_owned());
            lines.push("```".to_owned());
            lines.push("If there are multiple pending items, use:".to_owned());
            lines.push("```text".to_owned());
            lines.push("/approve 1".to_owned());
            lines.push("/approve-session 1".to_owned());
            lines.push("/deny 1".to_owned());
            lines.push("/cancel 1".to_owned());
            lines.push("```".to_owned());
            lines.join("\n")
        }
        PendingInteractionKind::FileChangeApproval => [
            "Codex wants to make file changes.",
            "",
            "If this is the only pending item, reply with:",
            "```text",
            "/approve",
            "```",
            "If there are multiple pending items, use:",
            "```text",
            "/approve 1",
            "/approve-session 1",
            "/deny 1",
            "/cancel 1",
            "```",
        ]
        .join("\n"),
        PendingInteractionKind::PermissionsApproval {
            permissions,
            reason,
        } => {
            let mut lines = vec![
                "Codex wants additional permissions:".to_owned(),
                "```json".to_owned(),
                permissions.to_string(),
                "```".to_owned(),
            ];
            if let Some(reason) = reason.as_deref().filter(|text| !text.trim().is_empty()) {
                lines.push(String::new());
                lines.push(reason.to_owned());
            }
            lines.push(String::new());
            lines.push("If this is the only pending item, reply with:".to_owned());
            lines.push("```text".to_owned());
            lines.push("/approve".to_owned());
            lines.push("```".to_owned());
            lines.push("If there are multiple pending items, use:".to_owned());
            lines.push("```text".to_owned());
            lines.push("/approve 1".to_owned());
            lines.push("/approve-session 1".to_owned());
            lines.push("/deny 1".to_owned());
            lines.push("/cancel 1".to_owned());
            lines.push("```".to_owned());
            lines.join("\n")
        }
        PendingInteractionKind::UserInputRequest { questions } => {
            let mut lines = Vec::new();
            for (index, question) in questions.iter().enumerate() {
                let header = question
                    .get("header")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Question");
                let body = question
                    .get("question")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(missing question text)");
                lines.push(format!("**{}. {}**", index + 1, header));
                lines.push(body.to_owned());
                if let Some(options) = question
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                {
                    let coded_options = render_coded_options(options);
                    if !coded_options.is_empty() {
                        lines.push("```text".to_owned());
                        lines.extend(coded_options);
                        lines.push("```".to_owned());
                    }
                }
                lines.push(String::new());
            }
            if questions.iter().all(|question| {
                question
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                    .is_some()
            }) {
                lines.push("Choose from the buttons below. Use `Other` if you want to type your own answer.".to_owned());
            } else if questions.len() == 1 {
                lines.push("Reply like this:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/reply your answer".to_owned());
                lines.push("```".to_owned());
                lines.push("If there are multiple pending items, use:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/reply 1 your answer".to_owned());
                lines.push("```".to_owned());
            } else {
                lines.push("Reply one question at a time like this:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/reply <question_id> your answer".to_owned());
                lines.push("```".to_owned());
                lines.push("If there are multiple pending items, use:".to_owned());
                lines.push("```text".to_owned());
                lines.push("/reply 1 <question_id> your answer".to_owned());
                lines.push("```".to_owned());
            }
            lines.join("\n")
        }
    }
}

fn pending_card_action_sections_v2(
    conversation_id: &str,
    summary: &PendingInteractionSummary,
) -> Vec<Vec<serde_json::Value>> {
    match &summary.kind {
        PendingInteractionKind::CommandApproval { .. }
        | PendingInteractionKind::FileChangeApproval
        | PendingInteractionKind::PermissionsApproval { .. } => vec![vec![
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/approve {}", summary.token),
                "Approve",
                "primary",
            ),
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/approve-session {}", summary.token),
                "Approve for session",
                "default",
            ),
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/deny {}", summary.token),
                "Deny",
                "danger",
            ),
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/cancel {}", summary.token),
                "Cancel",
                "default",
            ),
        ]],
        PendingInteractionKind::UserInputRequest { questions } => {
            let mut sections = Vec::new();

            for question in questions {
                let Some(question_id) = question.get("id").and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let Some(options) = question
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                else {
                    continue;
                };

                let buttons = options
                    .iter()
                    .enumerate()
                    .filter_map(|(index, option)| {
                        option
                            .get("label")
                            .and_then(serde_json::Value::as_str)
                            .map(|label| (index, label))
                    })
                    .map(|(index, label)| {
                        let command = if questions.len() == 1 {
                            format!("/reply {} {}", summary.token, label)
                        } else {
                            format!("/reply {} {} {}", summary.token, question_id, label)
                        };
                        pending_card_button(
                            conversation_id,
                            &summary.token,
                            command,
                            &coded_option_button_label(index),
                            "primary",
                        )
                    })
                    .collect::<Vec<_>>();

                if buttons.is_empty() {
                    continue;
                }

                for chunk in buttons.chunks(5) {
                    sections.push(chunk.to_vec());
                }
            }

            if questions.len() == 1
                && questions[0]
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                    .is_some()
            {
                sections.push(vec![pending_card_button_with_behavior(
                    conversation_id,
                    &summary.token,
                    format!("/reply-other {}", summary.token),
                    "Other",
                    "default",
                    true,
                )]);
            }

            sections
        }
    }
}

fn multi_question_action_sections(
    conversation_id: &str,
    token: &str,
    question: &serde_json::Value,
) -> Vec<Vec<serde_json::Value>> {
    let Some(question_id) = question.get("id").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let Some(options) = question
        .get("options")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let action_key = pending_card_action_key(token, Some(question_id));
    let mut sections = Vec::new();
    let buttons = options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            option
                .get("label")
                .and_then(serde_json::Value::as_str)
                .map(|label| (index, label))
        })
        .map(|(index, label)| {
            pending_card_button_for_key(
                conversation_id,
                &action_key,
                token,
                format!("/reply {} {} {}", token, question_id, label),
                &coded_option_button_label(index),
                "primary",
            )
        })
        .collect::<Vec<_>>();

    for chunk in buttons.chunks(5) {
        sections.push(chunk.to_vec());
    }

    sections.push(vec![pending_card_button_with_behavior_for_key(
        conversation_id,
        &action_key,
        token,
        format!("/reply-other {} {}", token, question_id),
        "Other",
        "default",
        true,
    )]);

    sections
}

fn pending_card_header(summary: &PendingInteractionSummary) -> (&'static str, &'static str) {
    match &summary.kind {
        PendingInteractionKind::CommandApproval { .. } => ("Codex 命令审批", "orange"),
        PendingInteractionKind::FileChangeApproval => ("Codex 文件变更审批", "red"),
        PendingInteractionKind::PermissionsApproval { .. } => ("Codex 权限审批", "orange"),
        PendingInteractionKind::UserInputRequest { .. } => ("Codex 需要你选择", "blue"),
    }
}

fn pending_card_markdown(summary: &PendingInteractionSummary) -> String {
    match &summary.kind {
        PendingInteractionKind::CommandApproval { command, cwd, reason } => format!(
            "**Token:** `{}`\n**命令:** `{}`\n**目录:** `{}`\n**原因:** {}\n\n可以直接点下面的按钮，也可以继续用文本命令。",
            summary.token,
            command.as_deref().unwrap_or("(not provided)"),
            cwd.as_deref().unwrap_or("(not provided)"),
            reason.as_deref().unwrap_or("(not provided)")
        ),
        PendingInteractionKind::FileChangeApproval => format!(
            "**Token:** `{}`\nCodex 请求执行文件变更。\n\n可以直接点下面的按钮，也可以继续用文本命令。",
            summary.token
        ),
        PendingInteractionKind::PermissionsApproval { permissions, reason } => format!(
            "**Token:** `{}`\n**权限请求:** `{}`\n**原因:** {}\n\n可以直接点下面的按钮，也可以继续用文本命令。",
            summary.token,
            permissions,
            reason.as_deref().unwrap_or("(not provided)")
        ),
        PendingInteractionKind::UserInputRequest { questions } => {
            let mut lines = vec![format!("**Token:** `{}`", summary.token)];
            for (index, question) in questions.iter().enumerate() {
                let header = question
                    .get("header")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("问题");
                let body = question
                    .get("question")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(missing question text)");
                lines.push(format!("**{}. {}**", index + 1, header));
                lines.push(body.to_owned());
                if let Some(options) = question.get("options").and_then(serde_json::Value::as_array) {
                    let option_lines = options
                        .iter()
                        .enumerate()
                        .filter_map(|(i, option)| {
                            option
                                .get("label")
                                .and_then(serde_json::Value::as_str)
                                .map(|label| format!("{}. {}", i + 1, label))
                        })
                        .collect::<Vec<_>>();
                    if !option_lines.is_empty() {
                        lines.push(String::new());
                        lines.extend(option_lines);
                    }
                }
                lines.push(String::new());
            }
            lines.push("可以直接点按钮，也可以继续用 `/reply` 文本回复。".to_owned());
            lines.join("\n")
        }
    }
}

fn pending_card_actions(
    conversation_id: &str,
    summary: &PendingInteractionSummary,
) -> Vec<serde_json::Value> {
    match &summary.kind {
        PendingInteractionKind::CommandApproval { .. }
        | PendingInteractionKind::FileChangeApproval
        | PendingInteractionKind::PermissionsApproval { .. } => vec![
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/approve {}", summary.token),
                "批准",
                "primary",
            ),
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/approve-session {}", summary.token),
                "本会话批准",
                "default",
            ),
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/deny {}", summary.token),
                "拒绝",
                "danger",
            ),
            pending_card_button(
                conversation_id,
                &summary.token,
                format!("/cancel {}", summary.token),
                "取消",
                "default",
            ),
        ],
        PendingInteractionKind::UserInputRequest { questions } => {
            if questions.len() != 1 {
                return Vec::new();
            }
            let Some(options) = questions[0]
                .get("options")
                .and_then(serde_json::Value::as_array)
            else {
                return Vec::new();
            };
            if options.len() > 5 {
                return Vec::new();
            }

            options
                .iter()
                .filter_map(|option| option.get("label").and_then(serde_json::Value::as_str))
                .map(|label| {
                    pending_card_button(
                        conversation_id,
                        &summary.token,
                        format!("/reply {} {}", summary.token, label),
                        label,
                        "primary",
                    )
                })
                .collect()
        }
    }
}

fn pending_card_button(
    conversation_id: &str,
    token: &str,
    command: impl Into<String>,
    label: &str,
    button_type: &str,
) -> serde_json::Value {
    pending_card_button_with_behavior(conversation_id, token, command, label, button_type, true)
}

fn pending_card_button_with_behavior(
    conversation_id: &str,
    token: &str,
    command: impl Into<String>,
    label: &str,
    button_type: &str,
    delete_after_action: bool,
) -> serde_json::Value {
    pending_card_button_with_behavior_for_key(
        conversation_id,
        token,
        token,
        command,
        label,
        button_type,
        delete_after_action,
    )
}

fn pending_card_button_for_key(
    conversation_id: &str,
    action_key: &str,
    token: &str,
    command: impl Into<String>,
    label: &str,
    button_type: &str,
) -> serde_json::Value {
    pending_card_button_with_behavior_for_key(
        conversation_id,
        action_key,
        token,
        command,
        label,
        button_type,
        true,
    )
}

fn pending_card_button_with_behavior_for_key(
    conversation_id: &str,
    action_key: &str,
    token: &str,
    command: impl Into<String>,
    label: &str,
    button_type: &str,
    delete_after_action: bool,
) -> serde_json::Value {
    serde_json::json!({
        "tag": "button",
        "text": {
            "tag": "plain_text",
            "content": label,
        },
        "type": button_type,
        "value": {
            "kind": "codex_pending",
            "conversation_id": conversation_id,
            "action_key": action_key,
            "token": token,
            "command": command.into(),
            "selected_label": label,
            "delete_after_action": delete_after_action,
        }
    })
}

fn pending_card_action_key(token: &str, question_id: Option<&str>) -> String {
    match question_id {
        Some(question_id) => format!("{token}::{question_id}"),
        None => token.to_owned(),
    }
}

fn looks_like_markdown_notice_post(text: &str) -> bool {
    text.contains("```")
}

enum NoticeBlock {
    Text(String),
    Code(String),
}

fn render_coded_options(options: &[serde_json::Value]) -> Vec<String> {
    options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            option
                .get("label")
                .and_then(serde_json::Value::as_str)
                .map(|label| format!("{}：{label}", coded_option_label(index)))
        })
        .collect()
}

fn coded_option_button_label(index: usize) -> String {
    if index == 0 {
        format!("{}（Recommand）", coded_option_label(index))
    } else {
        coded_option_label(index)
    }
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

fn extract_service_id(url: &str) -> Option<i32> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == "service_id" {
            return value.parse().ok();
        }
    }
    None
}

fn build_ping_frame(service_id: i32) -> Vec<u8> {
    let frame = FeishuFrame {
        seq_id: 0,
        log_id: 0,
        service: service_id,
        method: FRAME_TYPE_CONTROL,
        headers: vec![FeishuHeader {
            key: HEADER_TYPE.to_owned(),
            value: MESSAGE_TYPE_PING.to_owned(),
        }],
        payload_encoding: String::new(),
        payload_type: String::new(),
        payload: Vec::new(),
        log_id_new: String::new(),
    };
    frame.encode_to_vec()
}

fn build_pong_frame(service_id: i32) -> Vec<u8> {
    let frame = FeishuFrame {
        seq_id: 0,
        log_id: 0,
        service: service_id,
        method: FRAME_TYPE_CONTROL,
        headers: vec![FeishuHeader {
            key: HEADER_TYPE.to_owned(),
            value: MESSAGE_TYPE_PONG.to_owned(),
        }],
        payload_encoding: String::new(),
        payload_type: String::new(),
        payload: Vec::new(),
        log_id_new: String::new(),
    };
    frame.encode_to_vec()
}

fn build_response_frame(
    mut frame: FeishuFrame,
    headers: FrameHeaders,
    response: ResponseEnvelope,
) -> Result<Vec<u8>> {
    frame.headers = headers.headers;
    frame.payload =
        serde_json::to_vec(&response).context("failed to encode feishu response payload")?;
    Ok(frame.encode_to_vec())
}

#[derive(Debug, Clone)]
struct CachedTenantToken {
    token: String,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct ConversationStreamState {
    draft_message_id: Option<String>,
}

struct CardActionInbound {
    message: InboundMessage,
    selected_label: Option<String>,
    token: Option<String>,
    action_key: Option<String>,
    open_message_id: Option<String>,
    delete_after_action: bool,
}

struct PendingCardSpec {
    action_key: String,
    card: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct EndpointResponse {
    code: i32,
    msg: Option<String>,
    data: Option<EndpointData>,
}

#[derive(Debug, Deserialize)]
struct EndpointData {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig", default)]
    client_config: EndpointClientConfig,
}

#[derive(Debug, Default, Deserialize)]
struct EndpointClientConfig {
    #[serde(rename = "PingInterval", default = "default_ping_interval")]
    ping_interval: i32,
}

#[derive(Debug, Deserialize)]
struct EventEnvelope {
    header: EventHeader,
    event: EventBody,
}

#[derive(Debug, Deserialize)]
struct EventHeader {
    event_type: String,
}

#[derive(Debug, Deserialize)]
struct EventBody {
    sender: Option<EventSender>,
    message: Option<EventMessage>,
    operator: Option<CardActionOperator>,
    action: Option<CardAction>,
    context: Option<CardActionContext>,
}

#[derive(Debug, Deserialize)]
struct EventSender {
    sender_id: Option<EventUserId>,
}

#[derive(Debug, Deserialize)]
struct EventUserId {
    user_id: Option<String>,
    open_id: Option<String>,
    union_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventMessage {
    chat_id: Option<String>,
    thread_id: Option<String>,
    message_type: Option<String>,
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CardActionOperator {
    tenant_key: Option<String>,
    user_id: Option<String>,
    open_id: String,
}

#[derive(Debug, Deserialize)]
struct CardAction {
    value: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CardActionContext {
    open_message_id: Option<String>,
    open_chat_id: Option<String>,
}

impl EventMessage {
    fn extract_text(&self) -> Result<Option<String>> {
        let Some(content) = self.content.as_ref() else {
            return Ok(None);
        };
        match self.message_type.as_deref() {
            Some("text") | None => {
                let content: TextContent = serde_json::from_str(content)
                    .context("failed to decode feishu text content json")?;
                Ok(content.text.filter(|text| !text.trim().is_empty()))
            }
            Some("post") => extract_post_text(content),
            Some(_) => extract_generic_text(content),
        }
    }
}

#[derive(Debug, Deserialize)]
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
        Value::Array(values) => {
            for child in values {
                collect_text_fragments_into(child, fragments);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Deserialize)]
struct TenantAccessTokenResponse {
    code: i32,
    msg: Option<String>,
    tenant_access_token: Option<String>,
    expire: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct FeishuApiResponse<T> {
    code: i32,
    msg: Option<String>,
    #[allow(dead_code)]
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct CreateOrUpdateMessageData {
    message_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponseEnvelope {
    code: i32,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    headers: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    toast: Option<ResponseToast>,
    #[serde(skip_serializing_if = "Option::is_none")]
    card: Option<ResponseCard>,
}

impl ResponseEnvelope {
    fn ok() -> Self {
        Self {
            code: 200,
            headers: HashMap::new(),
            toast: None,
            card: None,
        }
    }

    fn with_success_toast(content: impl Into<String>) -> Self {
        Self {
            code: 200,
            headers: HashMap::new(),
            toast: Some(ResponseToast {
                type_: "success".to_owned(),
                content: content.into(),
            }),
            card: None,
        }
    }

    fn with_success_card(content: impl Into<String>, card: serde_json::Value) -> Self {
        Self {
            code: 200,
            headers: HashMap::new(),
            toast: Some(ResponseToast {
                type_: "success".to_owned(),
                content: content.into(),
            }),
            card: Some(ResponseCard {
                type_: "card_json".to_owned(),
                data: card,
            }),
        }
    }
}

#[derive(Debug, Serialize)]
struct ResponseToast {
    #[serde(rename = "type")]
    type_: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ResponseCard {
    #[serde(rename = "type")]
    type_: String,
    data: serde_json::Value,
}

#[derive(Clone, PartialEq, Message)]
struct FeishuHeader {
    #[prost(string, tag = "1")]
    key: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, Message)]
struct FeishuFrame {
    #[prost(uint64, tag = "1")]
    seq_id: u64,
    #[prost(uint64, tag = "2")]
    log_id: u64,
    #[prost(int32, tag = "3")]
    service: i32,
    #[prost(int32, tag = "4")]
    method: i32,
    #[prost(message, repeated, tag = "5")]
    headers: Vec<FeishuHeader>,
    #[prost(string, tag = "6")]
    payload_encoding: String,
    #[prost(string, tag = "7")]
    payload_type: String,
    #[prost(bytes, tag = "8")]
    payload: Vec<u8>,
    #[prost(string, tag = "9")]
    log_id_new: String,
}

#[derive(Clone)]
struct FrameHeaders {
    headers: Vec<FeishuHeader>,
}

impl FrameHeaders {
    fn new(headers: Vec<FeishuHeader>) -> Self {
        Self { headers }
    }

    fn get_string(&self, key: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|header| header.key == key)
            .map(|header| header.value.clone())
    }

    fn with(mut self, key: &str, value: String) -> Self {
        if let Some(header) = self.headers.iter_mut().find(|header| header.key == key) {
            header.value = value;
        } else {
            self.headers.push(FeishuHeader {
                key: key.to_owned(),
                value,
            });
        }
        self
    }
}

impl From<FrameHeaders> for Vec<FeishuHeader> {
    fn from(value: FrameHeaders) -> Self {
        value.headers
    }
}

fn default_ping_interval() -> i32 {
    120
}

#[cfg(test)]
mod tests {
    use super::{
        build_multi_question_card, build_notice_post, coded_option_button_label,
        coded_option_label, extract_service_id, looks_like_markdown_notice_post,
        multi_question_action_sections, EventMessage,
    };
    use serde_json::json;

    #[test]
    fn extracts_service_id_from_ws_url() {
        let url = "wss://example.com/ws?device_id=abc&service_id=42";
        assert_eq!(extract_service_id(url), Some(42));
    }

    #[test]
    fn extracts_text_from_feishu_content_json() {
        let message = EventMessage {
            chat_id: Some("oc_test".to_owned()),
            thread_id: None,
            message_type: Some("text".to_owned()),
            content: Some("{\"text\":\"hello from feishu\"}".to_owned()),
        };
        assert_eq!(
            message.extract_text().unwrap(),
            Some("hello from feishu".to_owned())
        );
    }

    #[test]
    fn extracts_text_from_feishu_post_content_json() {
        let message = EventMessage {
            chat_id: Some("oc_test".to_owned()),
            thread_id: None,
            message_type: Some("post".to_owned()),
            content: Some(
                serde_json::json!({
                    "zh_cn": {
                        "title": "Code sample",
                        "content": [
                            [
                                { "tag": "text", "text": "Here is a code block:" }
                            ],
                            [
                                { "tag": "text", "text": "```rust\nfn main() {}\n```" }
                            ]
                        ]
                    }
                })
                .to_string(),
            ),
        };
        let extracted = message.extract_text().unwrap().expect("text");

        assert!(extracted.contains("Code sample"));
        assert!(extracted.contains("Here is a code block:"));
        assert!(extracted.contains("fn main() {}"));
    }

    #[test]
    fn multi_question_card_uses_plain_text_reply_example() {
        let question = json!({
            "id": "color_choice",
            "header": "Color",
            "question": "Type your color"
        });

        let card = build_multi_question_card("chat-1", "req-1", &question, 1, 2);
        let content = card["elements"][0]["content"]
            .as_str()
            .expect("markdown content");

        assert!(content.contains("```text"));
        assert!(content.contains("/reply color_choice your answer"));
        assert!(!content.contains("{\"color_choice\": [\"your answer\"]}"));
    }

    #[test]
    fn multi_question_action_buttons_use_plain_text_reply_commands() {
        let question = json!({
            "id": "color_choice",
            "options": [
                { "label": "Blue" },
                { "label": "Green" }
            ]
        });

        let sections = multi_question_action_sections("chat-1", "req-1", &question);
        let command = sections[0][0]["value"]["command"]
            .as_str()
            .expect("button command");

        assert_eq!(command, "/reply req-1 color_choice Blue");
        assert!(!command.contains('{'));
    }

    #[test]
    fn multi_question_card_lists_options_as_coded_block() {
        let question = json!({
            "id": "color_choice",
            "header": "Color",
            "question": "Pick one",
            "options": [
                { "label": "Blue" },
                { "label": "Green" }
            ]
        });

        let card = build_multi_question_card("chat-1", "req-1", &question, 1, 2);
        let content = card["elements"][0]["content"]
            .as_str()
            .expect("markdown content");

        assert!(content.contains("```text"));
        assert!(content.contains(&format!("{}：Blue", coded_option_label(0))));
        assert!(content.contains(&format!("{}：Green", coded_option_label(1))));
    }

    #[test]
    fn multi_question_action_buttons_use_coded_labels() {
        let question = json!({
            "id": "color_choice",
            "options": [
                { "label": "Blue" },
                { "label": "Green" }
            ]
        });

        let sections = multi_question_action_sections("chat-1", "req-1", &question);
        let first_label = sections[0][0]["text"]["content"]
            .as_str()
            .expect("first label");
        let second_label = sections[0][1]["text"]["content"]
            .as_str()
            .expect("second label");
        let other_label = sections[1][0]["text"]["content"]
            .as_str()
            .expect("other label");

        assert_eq!(first_label, coded_option_button_label(0));
        assert_eq!(second_label, coded_option_button_label(1));
        assert_eq!(other_label, "Other");
    }

    #[test]
    fn markdown_notice_detection_only_matches_code_fences() {
        assert!(looks_like_markdown_notice_post(
            "Reply like this:\n\n```text\n/reply your answer\n```"
        ));
        assert!(!looks_like_markdown_notice_post(
            "plain notice without fenced block"
        ));
    }

    #[test]
    fn notice_post_uses_text_and_code_block_sections() {
        let post = build_notice_post("Reply like this:\n\n```text\n/reply your answer\n```");
        let rows = post["zh_cn"]["content"].as_array().expect("post rows");

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0]["tag"], "text");
        assert_eq!(rows[0][0]["text"], "Reply like this:");
        assert_eq!(rows[1][0]["tag"], "code_block");
        assert_eq!(rows[1][0]["language"], "PLAIN_TEXT");
        assert_eq!(rows[1][0]["text"], "/reply your answer\n");
    }
}
