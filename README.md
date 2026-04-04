# codex-channel

Standalone Rust gateway that connects Codex CLI with IM platforms.

Current status:

- `console` adapter is implemented for local testing.
- `feishu` long-connection adapter is implemented.
- `codex app-server` mode is implemented for long-lived Codex threads, IM-routed approvals, and user-input prompts.
- Codex sessions are persisted per IM conversation, so the same chat can continue across reconnects.

## Layout

```text
config/console.example.toml   Console adapter example
config/feishu.example.toml    Feishu adapter example
config/feishu.production.example.toml macOS production example
src/main.rs                   Entry point
src/gateway.rs                Core routing and per-conversation session binding
src/codex.rs                  Codex CLI JSONL bridge
src/codex_app_server.rs       Codex app-server bridge
src/im/console.rs             Local console adapter
src/im/feishu.rs              Feishu long-connection adapter
src/session_store.rs          Persistent session store
```

## Run

Build the binary first:

```bash
cargo build --release
```

Then run `codex-channel` directly.

Console mode:

```bash
./target/release/codex-channel --config config/console.example.toml
```

Feishu mode:

```bash
./target/release/codex-channel --config ~/.config/codex-channel/feishu.production.toml
```

When `use_app_server = true`, `codex-channel` will start `codex app-server` itself by using the
configured `launcher`. You do not need to run a separate app-server daemon first.

On macOS (Apple Silicon), build natively with:

```bash
cargo build --release
```

The binary will be written to:

```bash
target/release/codex-channel
```

## Feishu Notes

The Feishu adapter does the following:

1. Calls `POST /callback/ws/endpoint` to get the long-connection websocket URL.
2. Connects to the websocket and reads protobuf binary frames.
3. Acknowledges events quickly, then forwards text messages into the gateway.
4. Uses `tenant_access_token/internal` plus `im/v1/messages` to send Codex replies back to Feishu.

Current behavior:

- Only text message events are forwarded to Codex.
- Normal Codex replies are sent back as text messages to the same `chat_id` or `thread_id`.
- Pending approvals and user-input requests are rendered as Feishu cards.
- After a card action or matching text command succeeds, the pending card is deleted.

## IM Commands

The gateway supports a few thread and workspace commands directly from IM:

```text
/threads
/thread new <name>
/thread use <name>
/cd <path>
/pwd
/mode
/mode plan
/mode default
/pending
/approve [token-or-number]
/approve-session [token-or-number]
/deny [token-or-number]
/cancel [token-or-number]
/reply [token-or-number] <answer>
```

Behavior:

- Each IM conversation can hold multiple named Codex threads.
- Each named thread keeps its own Codex session id and working directory.
- `/thread new <name>` creates a fresh logical thread and switches to it.
- `/thread use <name>` switches the current logical thread.
- `/cd <path>` updates the working directory for the current logical thread.
- `/pwd` shows the current thread, working directory, and session status.
- `/mode` shows the current collaboration mode for the thread.
- `/mode plan` switches the current thread into Codex Plan mode for subsequent turns.
- `/mode default` switches the current thread back to Codex Default mode for subsequent turns.
- `/pending` shows pending Codex approvals or user-input requests for the current thread.
- `/approve`, `/approve-session`, `/deny`, `/cancel`, and `/reply` respond to pending Codex requests.
- If there is only one pending request, you can omit the target entirely, for example `/approve` or `/reply use the second option`.
- If there are multiple pending requests, run `/pending` and then use the shown item number, for example `/approve 2`, `/deny 3`, or `/reply 1 use the second option`.
- For multi-question Codex prompts, answer one question at a time with `/reply <question_id> <answer>`, or `/reply 2 <question_id> <answer>` if there are multiple pending requests.
- Raw tokens like `req-3` still work, but they are no longer required for normal phone-friendly usage.
- `/review` is currently disabled.

## Config

Example Feishu config:

```toml
state_file = ".codex-channel/sessions.json"

[adapter]
type = "feishu"
app_id = "FEISHU_APP_ID"
app_secret = "FEISHU_APP_SECRET"
base_url = "https://open.feishu.cn"
reconnect_interval_secs = 5

[codex]
launcher = ["codex"]
working_directory = ".."
use_app_server = true
app_server_url = "ws://127.0.0.1:54321"
sandbox = "workspace-write"
ask_for_approval = "on-request"
search = false
http_proxy = "http://127.0.0.1:7890"
https_proxy = "http://127.0.0.1:7890"
all_proxy = ""
no_proxy = ""
model = "gpt-5.4"
profile = ""
skip_git_repo_check = true
full_auto = false
turn_timeout_secs = 90
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

If `codex-channel` runs as a background service, Codex may not see the same trusted-project state
as your interactive terminal. This is especially common on macOS when the gateway is started by
`launchd`. If you see trust-related exits for a repository that already works in a terminal, set:

```toml
[codex]
skip_git_repo_check = true
```

This is often the most reliable choice for service-style deployment. If you want to keep the check
enabled, make sure the gateway process sees the same `HOME`, `PATH`, and working directory path that
you used when trusting the repository manually.

`codex-channel` can also pass common Codex execution controls through to the child process:

```toml
[codex]
sandbox = "workspace-write"
ask_for_approval = "never"
search = false
additional_writable_dirs = ["/Users/your-name/data", "/Users/your-name/logs"]
```

If you want IM-routed approvals and `request_user_input`, switch to app-server mode:

```toml
[codex]
use_app_server = true
app_server_url = "ws://127.0.0.1:54321"
ask_for_approval = "on-request"
```

In this mode, `codex-channel` keeps a long-lived Codex app-server session and can forward pending
approvals or user-input requests into IM. This is the mode to use when you want to approve blocked
commands from Feishu instead of restarting `codex exec` for every turn.

Recommended service-mode behavior on macOS:

- If you stay on `exec` mode, use `ask_for_approval = "never"` for `launchd` services.
- If you enable `use_app_server = true`, you can use `ask_for_approval = "on-request"` and route
  approvals back through IM with `/approve`, `/deny`, `/cancel`, and `/reply`.
- Use `sandbox = "workspace-write"` or a stricter mode that matches your needs.
- Use `additional_writable_dirs` for any extra paths Codex should be allowed to edit.
- If you want the model to use built-in web search, set `search = true`.
- Set `HOME` and `PATH` explicitly in your `launchd` plist so Codex reads the same config and auth
  files you use in your terminal.

If `codex-channel` runs as a background service on macOS, prefer an absolute launcher path because
`launchd` often has a much smaller `PATH` than your interactive terminal:

```toml
[codex]
launcher = ["/Users/your-name/.cargo/bin/codex"]
```

Feishu credentials are expected to come from environment variables. The `app_id` and `app_secret`
fields hold the environment variable names, not the raw secret values:

```toml
[adapter]
type = "feishu"
app_id = "FEISHU_APP_ID"
app_secret = "FEISHU_APP_SECRET"
```

## Deployment

Feishu is currently a built-in adapter, not a separately loaded plugin. In practice that means:

- `codex-channel` is a single standalone gateway binary.
- The active IM adapter is selected by the config file.
- `feishu` and `console` are compiled in today, and more adapters can be added later.

For a production-style setup on macOS:

1. Copy `config/feishu.production.example.toml` to your own machine-specific location, such as `~/.config/codex-channel/feishu.production.toml`.
2. Set `FEISHU_APP_ID` and `FEISHU_APP_SECRET` in the process environment. The config file should reference those variable names instead of storing raw credentials.
3. Update `working_directory`, `state_file`, sandbox, approval policy, proxy values, and usually `skip_git_repo_check = true` for your machine.
4. Install the built binary somewhere stable, such as `~/.cargo/bin/codex-channel`.
5. Set `launcher` in the config to the absolute path of the `codex` executable if you start the gateway from `launchd`.
6. Keep `use_app_server = true` if you want IM-routed approvals. `codex-channel` will launch `codex app-server` itself and connect to the configured local websocket address.
7. Load a `launchd` job based on `deploy/macos/com.codex-channel.example.plist`.

The sample plist includes placeholder environment variables for clarity. On a real machine, replace them with your own values before loading it.
It also sets `HOME`, `PATH`, and `NO_PROXY`, which are usually important on macOS when the gateway
needs to find `codex`, reuse your Codex auth/config files, and connect to the local app-server
socket without proxying loopback traffic.

Useful `launchd` commands:

```bash
mkdir -p ~/.config/codex-channel
cp config/feishu.production.example.toml ~/.config/codex-channel/feishu.production.toml
cp deploy/macos/com.codex-channel.example.plist ~/Library/LaunchAgents/com.codex-channel.plist
launchctl unload ~/Library/LaunchAgents/com.codex-channel.plist 2>/dev/null || true
launchctl load ~/Library/LaunchAgents/com.codex-channel.plist
launchctl start com.codex-channel
```

If you change the plist later, unload and load it again:

```bash
launchctl unload ~/Library/LaunchAgents/com.codex-channel.plist
launchctl load ~/Library/LaunchAgents/com.codex-channel.plist
```

## Next Steps

- Add Telegram adapter
- Add richer Feishu message support
- Add reply-to-message or thread-specific outbound behavior when needed
