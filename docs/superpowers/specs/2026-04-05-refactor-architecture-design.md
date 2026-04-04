# Codex Channel Refactor Architecture Design

## Goal

Refactor `codex-channel` into a layered, modular architecture that keeps all existing configuration fields and IM commands backward compatible, while making it straightforward to add:

- new agent backends such as Cursor, Claude Code, and OpenCode
- new frontends such as Telegram and slash-command based transports

The refactor must move frontend-specific and backend-specific behavior out of the channel core, so the core becomes a clean orchestration layer instead of a knowledge dump for every integration detail.

## Non-Goals

This refactor does not aim to:

- redesign the external CLI
- remove or rename existing config fields
- remove or rename existing IM commands
- re-enable `/review`
- ship new third-party backends or frontends in the same change

Those can happen later as additive work once the architecture is ready.

## Requirements

### Compatibility

The refactor must preserve:

- current configuration files and field meanings
- current IM commands and their behavior
- current Feishu long-connection workflow
- current Codex `exec` and `app-server` support

New configuration or command surface may be added, but existing users should not need to rewrite their config or retrain their usage.

### Separation of Concerns

The refactor must enforce these boundaries:

- frontend-specific parsing, message rendering, cards, rich text, and callback behavior live in frontend modules
- backend-specific process management, protocol handling, approvals, and request/response mapping live in backend modules
- channel core only understands domain concepts such as conversation, thread, pending interaction, user command, outbound notice, and turn lifecycle

### Extensibility

The codebase should support additive extension:

- a new backend should be implemented by adding a backend module and registering it
- a new frontend should be implemented by adding a frontend module and registering it
- the channel core should not need integration-specific branching for every new adapter

### Code Quality

The result should be:

- modular
- strongly typed
- interface driven
- easy to test in isolation
- free of leaked transport/protocol details in core logic

## Current Problems

The current code works, but it concentrates too much responsibility in a few places:

- `src/gateway.rs` currently mixes channel orchestration, command parsing, state mutation, user-input draft management, backend interaction flow, and transport-oriented message construction
- `src/codex.rs` and `src/codex_app_server.rs` are tied specifically to Codex, but they are not behind a general backend abstraction
- `src/im/feishu.rs` contains a mix of Feishu transport logic and channel-specific interaction behavior
- core message types exist, but they are not yet sufficient to keep frontend/backend concerns completely separated

This makes the system harder to extend cleanly and increases the chance that each new integration adds more conditional logic into the center of the codebase.

## Chosen Architecture

The refactor will adopt a three-layer architecture with a thin application layer on top.

### 1. Domain Layer

The domain layer defines transport-agnostic and backend-agnostic types:

- conversations
- threads
- collaboration modes
- pending interactions
- user commands
- inbound/outbound channel messages
- channel events and turn results

This layer should not contain Feishu card JSON or Codex JSON-RPC payloads.

### 2. Backend Layer

The backend layer defines a common `AgentBackend` abstraction for agent engines.

It is responsible for:

- starting/resuming turns
- listing and resolving pending interactions
- mapping backend-native protocol events into domain events
- hiding protocol differences such as `exec` vs `app-server`

The initial backend family is Codex:

- `CodexExecBackend`
- `CodexAppServerBackend`

These should sit behind a Codex-specific factory, but still implement the generic backend trait so later backends can join the same contract.

### 3. Frontend Layer

The frontend layer defines a common `ChannelFrontend` abstraction for inbound/outbound transports.

It is responsible for:

- receiving raw transport events
- parsing transport input into domain `InboundMessage`
- rendering domain `OutboundMessage` into transport-native messages
- handling transport-native interaction affordances such as Feishu cards and callbacks

The initial frontend family is:

- `ConsoleFrontend`
- `FeishuFrontend`

### 4. Application Layer

The application layer coordinates use cases. This is the replacement for the overgrown `Gateway`.

Recommended services:

- `ConversationService`
- `ThreadService`
- `InteractionService`
- `ModeService`
- `TurnService`

These services should orchestrate domain state and call frontend/backend interfaces, but should not know transport protocol details or backend wire formats.

## Proposed Module Structure

The target layout should move toward something like:

```text
src/
  app/
    mod.rs
    gateway.rs
    services/
      conversation_service.rs
      interaction_service.rs
      thread_service.rs
      turn_service.rs
  backend/
    mod.rs
    traits.rs
    registry.rs
    codex/
      mod.rs
      exec_backend.rs
      app_server_backend.rs
      mapping.rs
  frontend/
    mod.rs
    traits.rs
    registry.rs
    console/
      mod.rs
    feishu/
      mod.rs
      transport.rs
      rendering.rs
      parsing.rs
  domain/
    mod.rs
    conversation.rs
    thread.rs
    command.rs
    interaction.rs
    message.rs
    mode.rs
  infra/
    mod.rs
    config/
    storage/
  main.rs
```

This structure is directional. The refactor can land incrementally, but it should head toward these boundaries.

## Object Responsibilities

### `Gateway`

The new gateway should become a narrow application coordinator:

- boot config
- build frontend/backend instances
- receive inbound messages
- route them into application services

It should no longer hold detailed command handling and business rules directly.

### `AgentBackend`

Responsibilities:

- `run_turn`
- `respond_to_pending`
- `list_pending_for_thread`
- capability flags such as collaboration-mode support or review support

Backends should own all backend-specific error interpretation.

### `ChannelFrontend`

Responsibilities:

- `run`
- `send`
- frontend-native rendering helpers kept private to the implementation

Frontends should own all frontend-specific “how should this look” decisions.

### Domain Models

Domain models should own their own validation and display-oriented semantics where it makes sense.

Examples:

- command target parsing rules
- pending interaction summaries
- mode display names
- thread alias validation

This reduces stringly-typed logic leaking across modules.

## Configuration Strategy

Configuration must stay backward compatible.

The existing top-level config shape remains valid, but internally it should be normalized into:

- core/channel config
- backend config
- frontend config

The loader may continue reading the old shape, but it should produce strongly typed internal config objects so backends and frontends only receive the subset they actually need.

This makes later additive support easier:

- new backend-specific config can be added without leaking into core logic
- new frontend-specific config can be added without bloating unrelated modules

## Command Handling Strategy

Existing IM commands remain valid. The parsing and execution flow should be split:

- command parsing becomes domain/application concern
- command rendering and transport behavior stay in frontend implementations

For example:

- `/reply-other` should remain a generic command in domain/application logic
- Feishu-specific card deletion or rich post rendering should stay in Feishu frontend code

## Migration Strategy

The refactor should be incremental, not a one-shot rewrite.

### Phase 1: Stabilize Domain and Interfaces

- define backend/frontend traits
- extract domain types from current `model.rs` and scattered helpers
- move parsing/validation helpers into domain-owned modules

### Phase 2: Extract Application Services

- break `Gateway` logic into services
- keep behavior the same while moving code
- preserve tests and command compatibility

### Phase 3: Adapt Codex Backend

- wrap current Codex logic behind `AgentBackend`
- keep both `exec` and `app-server` modes available
- move protocol-specific mapping fully into backend code

### Phase 4: Adapt Frontends

- wrap Console and Feishu behind `ChannelFrontend`
- split Feishu transport/parsing/rendering internally
- remove channel-core knowledge from Feishu-specific code

### Phase 5: Cleanup and Review

- remove dead helpers and duplicate message builders
- tighten tests around layer boundaries
- document how to add a new backend or frontend

## Testing Strategy

The refactor should preserve and extend the current test safety net.

The testing approach should include:

- domain-unit tests for parsing and validation
- backend-unit tests for protocol mapping
- frontend-unit tests for parsing and rendering
- application-layer tests for command routing and state transitions

The important change is not just more tests, but better placement of tests so each layer can be verified independently.

## Acceptance Criteria

The refactor is complete when:

1. Existing configs still load without changes.
2. Existing IM commands still work without changes.
3. Channel core no longer imports frontend-specific JSON structures or backend protocol payload structures.
4. Codex-specific logic is behind backend abstractions.
5. Feishu-specific logic is behind frontend abstractions.
6. Adding a new backend requires additive registration, not surgery in the core gateway.
7. Adding a new frontend requires additive registration, not surgery in the core gateway.
8. The resulting code is smaller in responsibility per file, with clearer type ownership and fewer cross-layer leaks.

## Recommendation

This refactor should be implemented as a staged internal re-architecture that preserves runtime behavior as much as possible at each step.

That gives us the best trade-off:

- clean enough to support future backends/frontends
- safe enough not to break today’s working Feishu + Codex flow
- incremental enough to keep the codebase continuously testable
