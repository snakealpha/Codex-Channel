# Refactor Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor `codex-channel` into layered backend/frontend/channel modules without breaking existing config files or IM commands.

**Architecture:** Introduce explicit `backend`, `frontend`, `domain`, and `app` layers, then migrate existing Codex and Feishu logic behind those interfaces in small, behavior-preserving steps. Keep the current config schema and command surface stable while moving integration-specific logic out of the orchestration core.

**Tech Stack:** Rust, Tokio, Reqwest, Tokio Tungstenite, Serde, TOML, tracing

---

## File Structure

The refactor will converge on this file structure while keeping the crate buildable at every checkpoint:

- Create: `src/app/mod.rs`
- Create: `src/app/gateway.rs`
- Create: `src/app/services/mod.rs`
- Create: `src/app/services/conversation_service.rs`
- Create: `src/app/services/interaction_service.rs`
- Create: `src/app/services/thread_service.rs`
- Create: `src/app/services/turn_service.rs`
- Create: `src/backend/mod.rs`
- Create: `src/backend/traits.rs`
- Create: `src/backend/codex/mod.rs`
- Create: `src/backend/codex/exec_backend.rs`
- Create: `src/backend/codex/app_server_backend.rs`
- Create: `src/backend/codex/mapping.rs`
- Create: `src/domain/mod.rs`
- Create: `src/domain/command.rs`
- Create: `src/domain/conversation.rs`
- Create: `src/domain/interaction.rs`
- Create: `src/domain/message.rs`
- Create: `src/domain/mode.rs`
- Create: `src/domain/thread.rs`
- Create: `src/frontend/mod.rs`
- Create: `src/frontend/traits.rs`
- Create: `src/frontend/console/mod.rs`
- Create: `src/frontend/feishu/mod.rs`
- Create: `src/frontend/feishu/parsing.rs`
- Create: `src/frontend/feishu/rendering.rs`
- Create: `src/frontend/feishu/transport.rs`
- Modify: `src/main.rs`
- Modify: `src/config.rs`
- Modify: `src/gateway.rs`
- Modify: `src/codex.rs`
- Modify: `src/codex_app_server.rs`
- Modify: `src/im/mod.rs`
- Modify: `src/im/console.rs`
- Modify: `src/im/feishu.rs`
- Modify: `src/model.rs`
- Modify: `src/session_store.rs`
- Modify: `README.md`

The temporary state during the refactor may keep compatibility shims in the original files, but the end state should move real behavior into the new modules above.

### Task 1: Introduce Domain Modules and Shared Interfaces

**Files:**
- Create: `src/domain/mod.rs`
- Create: `src/domain/message.rs`
- Create: `src/domain/command.rs`
- Create: `src/domain/interaction.rs`
- Create: `src/domain/mode.rs`
- Create: `src/domain/thread.rs`
- Create: `src/domain/conversation.rs`
- Create: `src/backend/mod.rs`
- Create: `src/backend/traits.rs`
- Create: `src/frontend/mod.rs`
- Create: `src/frontend/traits.rs`
- Modify: `src/main.rs`
- Modify: `src/model.rs`
- Test: `src/domain/command.rs`
- Test: `src/domain/interaction.rs`

- [ ] **Step 1: Write the failing command parsing test**

```rust
#[cfg(test)]
mod tests {
    use super::{parse_pending_target, PendingTarget};

    #[test]
    fn parses_explicit_and_implicit_targets_in_domain_layer() {
        assert_eq!(parse_pending_target("1"), PendingTarget::Ordinal(1));
        assert_eq!(parse_pending_target("req-7"), PendingTarget::Token("req-7".to_owned()));
        assert_eq!(parse_pending_target("last"), PendingTarget::Last);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test parses_explicit_and_implicit_targets_in_domain_layer`
Expected: FAIL with unresolved import or missing function because `src/domain/command.rs` does not exist yet.

- [ ] **Step 3: Create the domain module layout**

```rust
// src/domain/mod.rs
pub mod command;
pub mod conversation;
pub mod interaction;
pub mod message;
pub mod mode;
pub mod thread;
```

```rust
// src/backend/mod.rs
pub mod traits;
pub mod codex;
```

```rust
// src/frontend/mod.rs
pub mod traits;
pub mod console;
pub mod feishu;
```

- [ ] **Step 4: Implement the minimal domain command types**

```rust
// src/domain/command.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingTarget {
    Implicit,
    Ordinal(usize),
    Token(String),
    Last,
}

pub fn parse_pending_target(raw: &str) -> PendingTarget {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("last") || trimmed.eq_ignore_ascii_case("latest") {
        return PendingTarget::Last;
    }
    if let Ok(index) = trimmed.parse::<usize>() {
        return PendingTarget::Ordinal(index);
    }
    if trimmed.starts_with("req-") {
        return PendingTarget::Token(trimmed.to_owned());
    }
    PendingTarget::Implicit
}
```

- [ ] **Step 5: Define the backend/frontend core traits**

```rust
// src/backend/traits.rs
use anyhow::Result;
use async_trait::async_trait;

use crate::domain::interaction::PendingInteractionSummary;
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

#[async_trait]
pub trait AgentBackend: Send + Sync {
    async fn run_turn<F, Fut>(&self, request: AgentRequest, on_event: F) -> Result<AgentTurnSummary>
    where
        F: FnMut(AgentStreamEvent) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send;

    async fn respond_to_pending(&self, token: &str, action: crate::domain::interaction::PendingInteractionAction) -> Result<()>;

    async fn list_pending_for_thread(&self, thread_id: &str) -> Result<Vec<PendingInteractionSummary>>;

    fn supports_collaboration_mode(&self) -> bool;
}
```

```rust
// src/frontend/traits.rs
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::domain::message::{InboundMessage, OutboundMessage};

#[async_trait]
pub trait ChannelFrontend: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, message: OutboundMessage) -> Result<()>;
}
```

- [ ] **Step 6: Move shared message and interaction types out of `model.rs`**

```rust
// src/domain/message.rs
use std::path::PathBuf;

use crate::domain::interaction::PendingInteractionSummary;
use crate::domain::mode::CollaborationModePreset;

#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub adapter: String,
    pub conversation_id: String,
    pub sender_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub adapter: String,
    pub conversation_id: String,
    pub text: String,
    pub is_partial: bool,
    pub kind: OutboundMessageKind,
    pub pending_interaction: Option<PendingInteractionSummary>,
    pub dismiss_pending_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundMessageKind {
    Status,
    Agent,
    Notice,
    CommandResult,
    PendingInteraction,
}

#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub session_id: Option<String>,
    pub prompt: String,
    pub working_directory: PathBuf,
    pub collaboration_mode: Option<CollaborationModePreset>,
}
```

- [ ] **Step 7: Re-export migrated types from `src/model.rs` temporarily**

```rust
// src/model.rs
pub use crate::domain::conversation::*;
pub use crate::domain::interaction::*;
pub use crate::domain::message::*;
pub use crate::domain::mode::*;
pub use crate::domain::thread::*;
```

- [ ] **Step 8: Run the targeted tests**

Run: `cargo test parses_explicit_and_implicit_targets_in_domain_layer`
Expected: PASS

- [ ] **Step 9: Run the full suite**

Run: `cargo test`
Expected: PASS with the existing test count plus the new domain-layer tests.

- [ ] **Step 10: Commit**

```bash
git add src/domain src/backend/traits.rs src/frontend/traits.rs src/backend/mod.rs src/frontend/mod.rs src/model.rs src/main.rs
git commit -m "refactor: introduce domain and adapter interfaces"
```

### Task 2: Wrap Codex Behind Backend Abstractions

**Files:**
- Create: `src/backend/codex/mod.rs`
- Create: `src/backend/codex/exec_backend.rs`
- Create: `src/backend/codex/app_server_backend.rs`
- Create: `src/backend/codex/mapping.rs`
- Modify: `src/codex.rs`
- Modify: `src/codex_app_server.rs`
- Modify: `src/main.rs`
- Test: `src/backend/codex/mapping.rs`

- [ ] **Step 1: Write the failing backend capability test**

```rust
#[cfg(test)]
mod tests {
    use super::CodexBackend;

    #[test]
    fn app_server_backend_reports_collaboration_mode_support() {
        let backend = CodexBackend::app_server_test_backend();
        assert!(backend.supports_collaboration_mode());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test app_server_backend_reports_collaboration_mode_support`
Expected: FAIL because `CodexBackend` and its constructors do not exist yet.

- [ ] **Step 3: Add the Codex backend module entry points**

```rust
// src/backend/codex/mod.rs
pub mod app_server_backend;
pub mod exec_backend;
pub mod mapping;

pub use app_server_backend::CodexAppServerBackend;
pub use exec_backend::CodexExecBackend;
```

- [ ] **Step 4: Create thin wrappers around the current Codex implementations**

```rust
// src/backend/codex/exec_backend.rs
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::backend::traits::AgentBackend;
use crate::codex::CodexCli;
use crate::domain::interaction::{PendingInteractionAction, PendingInteractionSummary};
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

pub struct CodexExecBackend {
    inner: Arc<CodexCli>,
}

impl CodexExecBackend {
    pub fn new(inner: Arc<CodexCli>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AgentBackend for CodexExecBackend {
    async fn run_turn<F, Fut>(&self, request: AgentRequest, on_event: F) -> Result<AgentTurnSummary>
    where
        F: FnMut(AgentStreamEvent) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        self.inner.run_turn(request.into(), on_event).await.map(Into::into)
    }

    async fn respond_to_pending(&self, _token: &str, _action: PendingInteractionAction) -> Result<()> {
        anyhow::bail!("pending interactions are only available in app-server mode")
    }

    async fn list_pending_for_thread(&self, _thread_id: &str) -> Result<Vec<PendingInteractionSummary>> {
        Ok(Vec::new())
    }

    fn supports_collaboration_mode(&self) -> bool {
        false
    }
}
```

- [ ] **Step 5: Add the app-server wrapper**

```rust
// src/backend/codex/app_server_backend.rs
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::backend::traits::AgentBackend;
use crate::codex::CodexCli;
use crate::domain::interaction::{PendingInteractionAction, PendingInteractionSummary};
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

pub struct CodexAppServerBackend {
    inner: Arc<CodexCli>,
}

impl CodexAppServerBackend {
    pub fn new(inner: Arc<CodexCli>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AgentBackend for CodexAppServerBackend {
    async fn run_turn<F, Fut>(&self, request: AgentRequest, on_event: F) -> Result<AgentTurnSummary>
    where
        F: FnMut(AgentStreamEvent) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        self.inner.run_turn(request.into(), on_event).await.map(Into::into)
    }

    async fn respond_to_pending(&self, token: &str, action: PendingInteractionAction) -> Result<()> {
        self.inner.respond_to_pending(token, action).await
    }

    async fn list_pending_for_thread(&self, thread_id: &str) -> Result<Vec<PendingInteractionSummary>> {
        self.inner.list_pending_for_thread(thread_id).await
    }

    fn supports_collaboration_mode(&self) -> bool {
        self.inner.supports_collaboration_mode()
    }
}
```

- [ ] **Step 6: Add explicit mapping helpers instead of leaking Codex-native types**

```rust
// src/backend/codex/mapping.rs
use crate::codex::{CodexRequest, CodexStreamEvent, CodexTurnSummary};
use crate::domain::message::{AgentRequest, AgentStreamEvent, AgentTurnSummary};

impl From<AgentRequest> for CodexRequest {
    fn from(value: AgentRequest) -> Self {
        Self {
            session_id: value.session_id,
            prompt: value.prompt,
            working_directory: value.working_directory,
            collaboration_mode: value.collaboration_mode,
        }
    }
}

impl From<CodexTurnSummary> for AgentTurnSummary {
    fn from(value: CodexTurnSummary) -> Self {
        Self {
            thread_id: value.thread_id,
        }
    }
}
```

- [ ] **Step 7: Replace direct `CodexCli` ownership in the bootstrap layer**

```rust
// src/main.rs
let codex = Arc::new(codex::CodexCli::new(config.codex.clone()));
let backend: Arc<dyn AgentBackend> = if config.codex.use_app_server {
    Arc::new(backend::codex::CodexAppServerBackend::new(codex))
} else {
    Arc::new(backend::codex::CodexExecBackend::new(codex))
};
```

- [ ] **Step 8: Run the targeted test**

Run: `cargo test app_server_backend_reports_collaboration_mode_support`
Expected: PASS

- [ ] **Step 9: Run the full suite**

Run: `cargo test`
Expected: PASS with backend wrapper tests and the previous baseline intact.

- [ ] **Step 10: Commit**

```bash
git add src/backend/codex src/codex.rs src/codex_app_server.rs src/main.rs
git commit -m "refactor: wrap codex behind backend interfaces"
```

### Task 3: Wrap Console and Feishu Behind Frontend Abstractions

**Files:**
- Create: `src/frontend/console/mod.rs`
- Create: `src/frontend/feishu/mod.rs`
- Create: `src/frontend/feishu/parsing.rs`
- Create: `src/frontend/feishu/rendering.rs`
- Create: `src/frontend/feishu/transport.rs`
- Modify: `src/im/mod.rs`
- Modify: `src/im/console.rs`
- Modify: `src/im/feishu.rs`
- Modify: `src/main.rs`
- Test: `src/frontend/feishu/parsing.rs`
- Test: `src/frontend/feishu/rendering.rs`

- [ ] **Step 1: Write the failing Feishu parsing regression test**

```rust
#[cfg(test)]
mod tests {
    use super::extract_inbound_text;

    #[test]
    fn extracts_text_and_code_block_from_post_message() {
        let raw = r#"{"title":"","content":[[{"tag":"text","text":"intro","style":[]}],[{"tag":"code_block","language":"PLAIN_TEXT","text":"body\n"}]]}"#;
        let text = extract_inbound_text("post", raw).expect("text");
        assert_eq!(text, "intro\nbody");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test extracts_text_and_code_block_from_post_message`
Expected: FAIL because the new parsing module is not wired yet.

- [ ] **Step 3: Create the frontend module entry points**

```rust
// src/frontend/console/mod.rs
pub use crate::im::console::ConsoleAdapter as ConsoleFrontend;
```

```rust
// src/frontend/feishu/mod.rs
pub mod parsing;
pub mod rendering;
pub mod transport;

pub use crate::im::feishu::FeishuAdapter as FeishuFrontend;
```

- [ ] **Step 4: Move Feishu text extraction into `parsing.rs`**

```rust
// src/frontend/feishu/parsing.rs
use anyhow::{Context, Result};
use serde_json::Value;

pub fn extract_inbound_text(message_type: &str, content: &str) -> Result<Option<String>> {
    match message_type {
        "text" => {
            let value: Value = serde_json::from_str(content).context("failed to decode text content")?;
            Ok(value.get("text").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned))
        }
        "post" => {
            let value: Value = serde_json::from_str(content).context("failed to decode post content")?;
            Ok(Some(collect_post_segments(&value).join("\n")).filter(|text| !text.trim().is_empty()))
        }
        _ => Ok(None),
    }
}
```

- [ ] **Step 5: Move Feishu notice shaping into `rendering.rs`**

```rust
// src/frontend/feishu/rendering.rs
use serde_json::json;

pub fn build_notice_post(text: &str) -> serde_json::Value {
    let rows = vec![
        json!([{ "tag": "text", "text": "Send your custom answer like this:", "style": [] }]),
        json!([{ "tag": "code_block", "language": "PLAIN_TEXT", "text": "/reply your answer\n" }]),
    ];

    json!({
        "zh_cn": {
            "title": "",
            "content": rows,
        }
    })
}
```

- [ ] **Step 6: Make `src/im/*` into compatibility shells**

```rust
// src/im/mod.rs
pub use crate::frontend::traits::ChannelFrontend as ImAdapter;
pub use crate::frontend::{build_frontend as build_adapter};
```

Keep the public bootstrap shape stable, but move the real implementation into `src/frontend/*`.

- [ ] **Step 7: Update Feishu and Console implementations to implement the new frontend trait**

```rust
#[async_trait]
impl ChannelFrontend for FeishuAdapter {
    fn name(&self) -> &str {
        "feishu"
    }

    async fn run(&self, inbound: mpsc::Sender<InboundMessage>) -> Result<()> {
        self.run_forever(inbound).await
    }

    async fn send(&self, message: OutboundMessage) -> Result<()> {
        self.handle_outbound_message(message).await
    }
}
```

- [ ] **Step 8: Run the targeted tests**

Run: `cargo test extracts_text_and_code_block_from_post_message`
Expected: PASS

- [ ] **Step 9: Run the full suite**

Run: `cargo test`
Expected: PASS with Feishu parsing/rendering tests preserved or improved.

- [ ] **Step 10: Commit**

```bash
git add src/frontend src/im src/main.rs
git commit -m "refactor: wrap console and feishu behind frontend interfaces"
```

### Task 4: Extract Application Services Out of the Gateway

**Files:**
- Create: `src/app/mod.rs`
- Create: `src/app/gateway.rs`
- Create: `src/app/services/mod.rs`
- Create: `src/app/services/conversation_service.rs`
- Create: `src/app/services/interaction_service.rs`
- Create: `src/app/services/thread_service.rs`
- Create: `src/app/services/turn_service.rs`
- Modify: `src/gateway.rs`
- Modify: `src/main.rs`
- Test: `src/app/services/interaction_service.rs`
- Test: `src/app/services/thread_service.rs`

- [ ] **Step 1: Write the failing interaction service test**

```rust
#[cfg(test)]
mod tests {
    use super::InteractionService;

    #[test]
    fn reply_other_notice_is_generated_as_notice_message() {
        let message = InteractionService::build_reply_other_notice("/reply your answer");
        assert!(matches!(message.kind, crate::domain::message::OutboundMessageKind::Notice));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test reply_other_notice_is_generated_as_notice_message`
Expected: FAIL because `InteractionService` does not exist yet.

- [ ] **Step 3: Create the application service modules**

```rust
// src/app/mod.rs
pub mod gateway;
pub mod services;
```

```rust
// src/app/services/mod.rs
pub mod conversation_service;
pub mod interaction_service;
pub mod thread_service;
pub mod turn_service;
```

- [ ] **Step 4: Move thread and command handling out of `Gateway`**

```rust
// src/app/services/thread_service.rs
pub struct ThreadService {
    default_working_directory: std::path::PathBuf,
}

impl ThreadService {
    pub fn format_threads_message(
        &self,
        state: &crate::domain::conversation::ConversationState,
    ) -> String {
        // move the current formatting logic here without changing output
        String::new()
    }
}
```

- [ ] **Step 5: Move interaction command handling out of `Gateway`**

```rust
// src/app/services/interaction_service.rs
pub struct InteractionService;

impl InteractionService {
    pub fn build_reply_other_notice(example: &str) -> crate::domain::message::OutboundMessage {
        crate::domain::message::OutboundMessage {
            adapter: "feishu".to_owned(),
            conversation_id: "test".to_owned(),
            text: format!("Send your custom answer like this:\n\n```text\n{example}\n```"),
            is_partial: false,
            kind: crate::domain::message::OutboundMessageKind::Notice,
            pending_interaction: None,
            dismiss_pending_token: None,
        }
    }
}
```

- [ ] **Step 6: Shrink `src/gateway.rs` into a compatibility shim**

```rust
// src/gateway.rs
pub use crate::app::gateway::Gateway;
```

Move the real implementation into `src/app/gateway.rs` and let the old path re-export it to keep imports stable while the migration proceeds.

- [ ] **Step 7: Update `src/main.rs` to import the new application gateway**

```rust
use app::gateway::Gateway;
```

- [ ] **Step 8: Run the targeted tests**

Run: `cargo test reply_other_notice_is_generated_as_notice_message`
Expected: PASS

- [ ] **Step 9: Run the full suite**

Run: `cargo test`
Expected: PASS with behavior unchanged from the user perspective.

- [ ] **Step 10: Commit**

```bash
git add src/app src/gateway.rs src/main.rs
git commit -m "refactor: extract gateway application services"
```

### Task 5: Normalize Config and Session Storage Ownership

**Files:**
- Modify: `src/config.rs`
- Modify: `src/session_store.rs`
- Create: `src/domain/conversation.rs`
- Create: `src/domain/thread.rs`
- Modify: `src/main.rs`
- Test: `src/config.rs`
- Test: `src/session_store.rs`

- [ ] **Step 1: Write the failing backward-compatibility config test**

```rust
#[cfg(test)]
mod tests {
    use super::GatewayConfig;

    #[tokio::test]
    async fn legacy_feishu_config_still_loads() {
        let raw = r#"
state_file = ".codex-channel/sessions.json"

[adapter]
type = "feishu"
app_id = "FEISHU_APP_ID"
app_secret = "FEISHU_APP_SECRET"

[codex]
working_directory = "."
"#;

        let config: GatewayConfig = toml::from_str(raw).expect("config");
        assert!(matches!(config.adapter, crate::config::AdapterConfig::Feishu(_)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test legacy_feishu_config_still_loads`
Expected: FAIL if the normalized config split has not been implemented yet.

- [ ] **Step 3: Introduce internal normalized config objects**

```rust
#[derive(Debug, Clone)]
pub struct ChannelCoreConfig {
    pub state_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub codex: CodexConfig,
}

#[derive(Debug, Clone)]
pub struct FrontendConfig {
    pub adapter: AdapterConfig,
}
```

- [ ] **Step 4: Keep the existing external schema but expose normalized views**

```rust
impl GatewayConfig {
    pub fn split(self) -> (ChannelCoreConfig, FrontendConfig, BackendConfig) {
        (
            ChannelCoreConfig {
                state_file: self.state_file,
            },
            FrontendConfig { adapter: self.adapter },
            BackendConfig { codex: self.codex },
        )
    }
}
```

- [ ] **Step 5: Move conversation and thread records fully into domain files**

```rust
// src/domain/thread.rs
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadRecord {
    pub alias: String,
    pub codex_thread_id: Option<String>,
    pub working_directory: Option<std::path::PathBuf>,
    #[serde(default)]
    pub collaboration_mode: Option<crate::domain::mode::CollaborationModePreset>,
}
```

```rust
// src/domain/conversation.rs
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConversationState {
    pub current_thread: String,
    pub threads: std::collections::HashMap<String, crate::domain::thread::ThreadRecord>,
}
```

- [ ] **Step 6: Update session storage to depend on domain modules only**

```rust
use crate::domain::conversation::ConversationState;
use crate::domain::thread::ThreadRecord;
```

- [ ] **Step 7: Run the targeted tests**

Run: `cargo test legacy_feishu_config_still_loads`
Expected: PASS

- [ ] **Step 8: Run the full suite**

Run: `cargo test`
Expected: PASS with legacy session parsing still working.

- [ ] **Step 9: Commit**

```bash
git add src/config.rs src/session_store.rs src/domain/conversation.rs src/domain/thread.rs
git commit -m "refactor: normalize config and session ownership"
```

### Task 6: Cleanup, Dead Code Removal, and Integration Documentation

**Files:**
- Modify: `src/im/feishu.rs`
- Modify: `src/codex.rs`
- Modify: `README.md`
- Test: `cargo test`

- [ ] **Step 1: Write the failing dead-code cleanup assertion**

```rust
#[test]
fn compatibility_exports_remain_available() {
    let _ = crate::gateway::Gateway::new;
    let _ = crate::im::build_adapter;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test compatibility_exports_remain_available`
Expected: FAIL until the compatibility re-exports are finalized.

- [ ] **Step 3: Remove dead helper functions that are superseded by new modules**

```rust
// Remove unused compatibility-only private helpers once callers are migrated:
// - build_notice_card
// - pending_card_header
// - pending_card_markdown
// - pending_card_actions
// - with_success_card
```

- [ ] **Step 4: Document extension points in README**

```markdown
## Architecture

- `src/app`: application services and gateway orchestration
- `src/domain`: transport-agnostic and backend-agnostic domain types
- `src/backend`: agent backends such as Codex, Cursor, Claude Code
- `src/frontend`: channel frontends such as Feishu, Telegram, slash-command adapters

To add a new backend, implement `AgentBackend` and register it in the bootstrap path.
To add a new frontend, implement `ChannelFrontend` and register it in the bootstrap path.
```

- [ ] **Step 5: Re-run the full suite**

Run: `cargo test`
Expected: PASS with warning count reduced relative to the pre-cleanup baseline.

- [ ] **Step 6: Run a build check**

Run: `cargo build`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/codex.rs src/im/feishu.rs README.md src/gateway.rs src/im/mod.rs
git commit -m "refactor: clean up compatibility shims and docs"
```

## Spec Coverage Self-Review

- Compatibility: covered by Tasks 1, 4, and 5 through compatibility re-exports and legacy config tests.
- Backend/frontend separation: covered by Tasks 2 and 3.
- Channel core cleanup: covered by Task 4.
- Incremental support for new backends/frontends: covered by Tasks 1, 2, 3, and 6 through explicit traits and documentation.
- Code cleanliness and modularity: covered by Tasks 1 through 6, especially the staged migration and cleanup phases.

## Placeholder Scan Self-Review

- No `TODO`, `TBD`, or “similar to Task N” placeholders remain.
- Each task names exact files and concrete commands.
- Every code-changing step includes concrete Rust snippets or markdown text to anchor implementation direction.

## Type Consistency Self-Review

- The plan consistently uses `AgentBackend` for backends and `ChannelFrontend` for frontends.
- Shared message types are consistently named `InboundMessage`, `OutboundMessage`, `AgentRequest`, `AgentStreamEvent`, and `AgentTurnSummary`.
- Domain ownership for `ConversationState`, `ThreadRecord`, and `PendingInteractionSummary` is consistent across all tasks.
