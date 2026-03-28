use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::{interval, sleep};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{error, info, warn};

use crate::config::FeishuAdapterConfig;
use crate::im::ImAdapter;
use crate::model::{InboundMessage, OutboundMessage, OutboundMessageKind};

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
        let (socket, _) = connect_async(&endpoint.url)
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

        if event.header.event_type != "im.message.receive_v1" {
            return Ok(Some(build_response_frame(
                frame,
                FrameHeaders::from(headers)
                    .with(HEADER_BIZ_RT, start.elapsed().as_millis().to_string()),
                ResponseEnvelope::ok(),
            )?));
        }

        if let Some(message) = event.event.message.as_ref() {
            if message.message_type.as_deref() == Some("text") {
                if let Some(text) = message.extract_text()? {
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
                            anyhow!("gateway stopped before feishu message could be delivered")
                        })?;
                }
            }
        }

        let response_headers = FrameHeaders::from(headers)
            .with(HEADER_BIZ_RT, start.elapsed().as_millis().to_string());
        Ok(Some(build_response_frame(
            frame,
            response_headers,
            ResponseEnvelope::ok(),
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
                    self.upsert_stream_message(&message.conversation_id, &message.text, true)
                        .await
                } else {
                    self.upsert_stream_message(&message.conversation_id, &message.text, false)
                        .await
                }
            }
            OutboundMessageKind::Notice => {
                let keep_open = !message.text.starts_with("Codex failed:");
                info!(
                    conversation_id = message.conversation_id,
                    keep_open,
                    "upserting feishu notice message"
                );
                self.upsert_stream_message(&message.conversation_id, &message.text, keep_open)
                    .await?;
                Ok(())
            }
            OutboundMessageKind::CommandResult => {
                info!(
                    conversation_id = message.conversation_id,
                    "sending feishu command result message"
                );
                self.clear_stream_state(&message.conversation_id).await;
                let _ = self
                    .send_text_message(&message.conversation_id, &message.text)
                    .await?;
                Ok(())
            }
        }
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

        let status = response.status();
        let body: FeishuApiResponse<CreateOrUpdateMessageData> = response
            .json()
            .await
            .context("failed to parse feishu send-message response")?;

        if !status.is_success() || body.code != 0 {
            bail!(
                "feishu send-message failed with http {} and code {}: {}",
                status,
                body.code,
                body.msg.unwrap_or_else(|| "unknown error".to_owned())
            );
        }

        body.data
            .and_then(|data| data.message_id)
            .ok_or_else(|| anyhow!("feishu send-message response missing message_id"))
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

        let status = response.status();
        let body: FeishuApiResponse<CreateOrUpdateMessageData> = response
            .json()
            .await
            .context("failed to parse feishu update-message response")?;

        if !status.is_success() || body.code != 0 {
            bail!(
                "feishu update-message failed with http {} and code {}: {}",
                status,
                body.code,
                body.msg.unwrap_or_else(|| "unknown error".to_owned())
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

fn resolve_secret_env(
    field_name: &str,
    env_name: Option<String>,
) -> Result<String> {
    let env_name = env_name
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow!("missing feishu {field_name}; configure it with an environment variable name"))?;

    let value = std::env::var(&env_name)
        .with_context(|| format!("failed to read env var `{env_name}` for feishu {field_name}"))?;
    if !value.trim().is_empty() {
        return Ok(value);
    }

    bail!("env var `{env_name}` for feishu {field_name} is empty")
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

impl EventMessage {
    fn extract_text(&self) -> Result<Option<String>> {
        let Some(content) = self.content.as_ref() else {
            return Ok(None);
        };
        let content: TextContent =
            serde_json::from_str(content).context("failed to decode feishu text content json")?;
        Ok(content.text.filter(|text| !text.trim().is_empty()))
    }
}

#[derive(Debug, Deserialize)]
struct TextContent {
    text: Option<String>,
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
}

impl ResponseEnvelope {
    fn ok() -> Self {
        Self {
            code: 200,
            headers: HashMap::new(),
        }
    }
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
    use super::{extract_service_id, EventMessage};

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
}
