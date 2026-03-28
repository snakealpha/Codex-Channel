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
```

Behavior:

- Each IM conversation can hold multiple named Codex threads.
- Each named thread keeps its own Codex session id and working directory.
- `/thread new <name>` creates a fresh logical thread and switches to it.
- `/thread use <name>` switches the current logical thread.
- `/cd <path>` updates the working directory for the current logical thread.
- `/pwd` shows the current thread, working directory, and session status.
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
3. Update `working_directory`, `state_file`, and proxy values for your machine.
4. Install the built binary somewhere stable, such as `~/.cargo/bin/codex-channel`.
5. Set `launcher` in the config to the absolute path of the `codex` executable if you start the gateway from `launchd`.
6. Load a `launchd` job based on `deploy/macos/com.codex-channel.example.plist`.

The sample plist includes placeholder environment variables for clarity. On a real machine, replace them with your own values before loading it.

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
