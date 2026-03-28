# codex-channel

Standalone Rust gateway that connects Codex CLI with IM platforms.

Current status:

- `console` adapter is implemented for local testing.
- `feishu` long-connection adapter is implemented.
- Codex sessions are persisted per IM conversation, so the same chat can continue with `codex exec resume`.

## Layout

```text
config/console.example.toml   Console adapter example
config/feishu.example.toml    Feishu adapter example
src/main.rs                   Entry point
src/gateway.rs                Core routing and per-conversation session binding
src/codex.rs                  Codex CLI JSONL bridge
src/im/console.rs             Local console adapter
src/im/feishu.rs              Feishu long-connection adapter
src/session_store.rs          Persistent session store
```

## Run

Console mode:

```bash
cargo run -- --config config/console.example.toml
```

Feishu mode:

```bash
cargo run -- --config config/feishu.local.toml
```

## Feishu Notes

The Feishu adapter does the following:

1. Calls `POST /callback/ws/endpoint` to get the long-connection websocket URL.
2. Connects to the websocket and reads protobuf binary frames.
3. Acknowledges events quickly, then forwards text messages into the gateway.
4. Uses `tenant_access_token/internal` plus `im/v1/messages` to send Codex replies back to Feishu.

Current behavior:

- Only text message events are forwarded to Codex.
- Replies are sent back as plain text messages to the same `chat_id` or `thread_id`.
- Replies are updated in place when possible, so one Feishu message can show thinking, reconnecting,
  and final output.

## IM Commands

The gateway supports a few thread and workspace commands directly from IM:

```text
/threads
/thread new <name>
/thread use <name>
/cd <path>
/pwd
/review
```

Behavior:

- Each IM conversation can hold multiple named Codex threads.
- Each named thread keeps its own Codex session id and working directory.
- `/thread new <name>` creates a fresh logical thread and switches to it.
- `/thread use <name>` switches the current logical thread.
- `/cd <path>` updates the working directory for the current logical thread.
- `/pwd` shows the current thread, working directory, and session status.
- `/review` runs against the current logical thread's working directory.

## Config

Example Feishu config:

```toml
state_file = ".codex-channel/sessions.json"

[adapter]
type = "feishu"
app_id = "cli_xxx"
app_secret = "xxx"
base_url = "https://open.feishu.cn"
reconnect_interval_secs = 5

[codex]
working_directory = ".."
http_proxy = "http://127.0.0.1:7890"
https_proxy = "http://127.0.0.1:7890"
skip_git_repo_check = false
full_auto = false
additional_writable_dirs = []
extra_args = []
```

If your network requires a proxy for Codex to reach OpenAI, configure it under `[codex]`:

```toml
[codex]
http_proxy = "http://127.0.0.1:7890"
https_proxy = "http://127.0.0.1:7890"
```

These values are injected into the `codex exec` child process as both uppercase and lowercase
proxy environment variables.

You can also use environment variables instead of inline secrets:

```toml
[adapter]
type = "feishu"
app_id_env = "FEISHU_APP_ID"
app_secret_env = "FEISHU_APP_SECRET"
```

## Next Steps

- Add Telegram adapter
- Add richer Feishu message support
- Add reply-to-message or thread-specific outbound behavior when needed
