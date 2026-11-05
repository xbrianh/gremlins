# Compaction Design

Server-side conversation compaction for `StandardConversation`, using
Anthropic's on-demand compaction API (`compact-2026-09-04` beta) with a
client-side fallback for other backends.

## Motivation

Agent conversations grow without bound as the model calls tools, reads files,
and reasons across many turns. Response quality degrades as context grows
("context rot"), and costs increase linearly with input tokens. Compaction
replaces older turns with a concise server-generated summary, keeping the
active context small while preserving key decisions, facts, and pending work.

Anthropic offers two compaction modes (both beta):

| Mode | Who triggers | Header | Parameter |
|---|---|---|---|
| On-demand | The application | `compact-2026-09-04` | `compaction: {"type": "summarize"}` |
| Threshold | The API, at a token count | `compact-2026-01-12` | `context_management.edits` |

We target **on-demand** compaction. It gives us full control over when
compaction occurs, supports keeping recent turns verbatim, and returns a
signed block we can persist and round-trip. Threshold compaction is a
future addition that can ride on the same infrastructure.

## Anthropic on-demand compaction API

### Request

`POST /v1/messages` with the `compact-2026-09-04` beta header.

The request carries the conversation to summarize and a `compaction` param:

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 4096,
  "system": "You are a helpful assistant...",
  "messages": [
    {"role": "user", "content": "Help me build a recipe app."},
    {"role": "assistant", "content": "Start with Recipe, Ingredient, Step..."}
  ],
  "compaction": {
    "type": "summarize",
    "instructions": "Preserve entity names, field decisions, and the latest open request."
  }
}
```

- `instructions` is optional (up to 16,384 chars). When omitted the API uses
  its own default summarization prompt. When provided it **replaces** the
  default entirely.
- `max_tokens` caps the summarization call (including any internal thinking).
  Allow several thousand tokens.
- `tools` and `thinking` settings are read by the summarizer but it never
  calls tools.
- Cannot be combined with `context_management` on the same request.
- The last assistant turn must not end in a tool call with no result.

### Response

HTTP 200 with `stop_reason: "compaction"` and a single compaction block:

```json
{
  "id": "msg_013Zva2CMHLNnXjNJJKqJ2EF",
  "type": "message",
  "role": "assistant",
  "model": "claude-sonnet-4-6",
  "content": [
    {
      "type": "compaction",
      "content": "Summary: user is designing a recipe app data model. Entities agreed: Recipe, Ingredient, Step, RecipeIngredient. User asked for field names for Recipe.",
      "signature": "EuYBCkQY...",
      "encrypted_content": null,
      "tool_changes": null
    }
  ],
  "stop_reason": "compaction",
  "usage": {
    "input_tokens": 0,
    "output_tokens": 0,
    "iterations": [
      {"type": "compaction", "input_tokens": 144, "output_tokens": 276}
    ]
  }
}
```

- `signature` is tamper-proof. The API rejects modified blocks with
  `400 compaction_signature_invalid`.
- `content` is the human-readable summary.
- Top-level `usage.input_tokens` and `output_tokens` are zero — compaction
  cost is only in `usage.iterations`.
- When no summary is produced (e.g. model called a tool, max_tokens exceeded),
  `content` is `null`. Caller keeps original messages and tries again later.

### Continuing from the block

In subsequent requests, place the returned block **first in `messages`** in
place of the messages it summarizes:

```json
{
  "messages": [
    {"role": "assistant", "content": [{"type": "compaction", "content": "Summary: ...", "signature": "..."}]},
    {"role": "assistant", "content": "For Recipe, use title, description, servings..."},
    {"role": "user", "content": "Now do the same for Ingredient."}
  ]
}
```

Rules:
- Send exactly one block per request, on every later request.
- The block must be first. Summarized messages before it → `400 compaction_block_misplaced`.
- Summarized messages after it are silently passed through — remove them.
- Send the beta header on every request carrying the block.

### Error handling

| stop_reason | Cause | Action |
|---|---|---|
| `"max_tokens"` | Summary cut off | Increase `max_tokens`, retry |
| `"tool_use"` | Model called a tool | Add instructions: "do not call tools" |
| `"refusal"` | Safeguards blocked | Continue without summary |
| `"end_turn"` | No text produced | Continue without summary |

HTTP errors:

| Error | Cause | Action |
|---|---|---|
| `529 compaction_unavailable` | Transient server issue | Retry |
| `400 compaction_block_misplaced` | Block not first in messages | Fix message ordering |
| `400 compaction_signature_invalid` | Block was modified | Send original block |
| `400` | Multiple compaction blocks | Send exactly one |

### Prompt caching with compaction

Add `cache_control: {"type": "ephemeral"}` on the compaction block. Also add
a breakpoint at the end of the system prompt so it stays cached when the
compaction summary changes:

```json
{
  "system": [{"type": "text", "text": "You are...", "cache_control": {"type": "ephemeral"}}],
  "messages": [
    {"role": "assistant", "content": [
      {"type": "compaction", "content": "...", "signature": "...", "cache_control": {"type": "ephemeral"}}
    ]}
  ]
}
```

### Models supporting on-demand compaction

On-demand compaction is available on Claude Sonnet 4.6 and later
models. Current-generation Haiku 4.5, Opus 4.5, and Sonnet 4.5 do **not**
support it. The examples in this document use `claude-sonnet-4-6`, a
supporting model.

Check via Models API: read `capabilities.compaction` with the beta header.

---

## Architecture

### Summary

```
render_turn() {
    if last_input_tokens > threshold {
        compact()
            → try backend.compact(messages)   // server-side
            → fallback: client_side_compact()  // summarization model call
            → swap block into history
    }
    // existing scratchpad/findings/turn-prefix logic
}
```

### Responsible layers

| Layer | Responsibility |
|---|---|
| YAML (`extra_params`) | `compaction.threshold`, `.keep-recent`, `.instructions` |
| `run.rs` | Extracts config, builds backend, constructs `CompactionConfig`, passes to `StandardConversation::new` |
| `Backend::compact()` | Default `Ok(None)`; `AnthropicBackend` overrides |
| `CompactionConfig` (struct) | Holds backend, threshold, keep-recent, instructions; exposes accessors + `async compact()` for server-side/client-side paths |
| `Conversation` trait | `render_turn` becomes `async`; adds `has_signed_compaction_block()` (no new backend-coupled methods) |
| `StandardConversation` | Holds `Option<CompactionConfig>` + `CompactionBlock`; owns `should_compact()`, `last_input_tokens()`, turn-boundary cut logic, and `has_signed_compaction_block()`; triggers and applies compaction in `render_turn` |

---

## Implementation details

### 1. New types (`protocol.rs`)

```rust
/// A signed compaction block from the Anthropic on-demand compaction API
/// (`compact-2026-09-04`). Round-tripped verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionBlock {
    pub content: String,
    pub signature: String,
    #[serde(default)]
    pub encrypted_content: Option<String>,
    #[serde(default)]
    pub tool_changes: Option<Vec<serde_json::Value>>,
}
```

### 2. `Backend` trait (`backend.rs`)

One new optional async method with a default no-op:

```rust
#[async_trait]
pub(crate) trait Backend: Send + Sync + Debug {
    // ... existing methods unchanged ...

    /// Request server-side compaction.
    /// Returns `Ok(None)` when the backend doesn't support compaction.
    async fn compact(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
        instructions: Option<&str>,
    ) -> Result<Option<CompactionBlock>, ClientError> {
        let _ = (messages, system_prompt, instructions);
        Ok(None)
    }
}
```

### 3. `AnthropicBackend::compact()` (`backends/anthropic.rs`)

Sends `POST /v1/messages` with the `compact-2026-09-04` beta header:

```rust
impl Backend for AnthropicBackend {
    async fn compact(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
        instructions: Option<&str>,
    ) -> Result<Option<CompactionBlock>, ClientError> {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), self.state.model.clone().into());
        body.insert("max_tokens".into(), 4096.into());
        body.insert("messages".into(), serde_json::to_value(messages)?);

        let mut compaction = serde_json::Map::new();
        compaction.insert("type".into(), "summarize".into());
        if let Some(inst) = instructions {
            compaction.insert("instructions".into(), inst.into());
        }
        body.insert("compaction".into(), compaction.into());

        if let Some(sys) = system_prompt {
            body.insert("system".into(), sys.into());
        }

        // Send through existing HTTP client (with auth, retry, etc.)
        // Parse response: check stop_reason == "compaction", extract block.
        // ...
    }
}
```

The call uses the backend's existing HTTP client, base URL, auth, and retry
logic. It bypasses rig because the response shape (`stop_reason: "compaction"`)
differs from normal completions.

### 4. `Conversation` trait (`conversation.rs`)

One change — `render_turn` becomes `async`. No backend-coupled methods are
added to the trait; `Conversation` stays purely about message history and
rendering:

```rust
#[async_trait]
pub(crate) trait Conversation: Send + Sync {
    // ... all existing methods unchanged ...

    /// True when a server-issued signed compaction block is active and must
    /// be sent with the beta header on ordinary completion requests.
    fn has_signed_compaction_block(&self) -> bool { false }

    /// Now async — may trigger compaction before building the turn.
    async fn render_turn(
        &mut self,
        nested: bool,
        scratchpad: Option<String>,
        system_prompt: Option<String>,
    ) -> TurnScript;
}
```

### 5. `CompactionConfig` struct (`conversation/standard.rs` or new module)

A concrete struct built by `run.rs` and handed to `StandardConversation`.
It owns the backend handle, all configuration, and the compaction logic.
`Conversation` never sees a backend.

```rust
/// Compaction strategy and execution. Built once in `run.rs` and passed
/// to `StandardConversation::new`. Holds an `Arc<dyn Backend>` so it is
/// cheap to clone.
pub(crate) struct CompactionConfig {
    backend: Arc<dyn Backend>,
    model_spec: String,                     // "anthropic/claude-sonnet-4-6"
    threshold: u64,                         // token threshold for triggering
    keep_recent: usize,                     // user+assistant pairs to preserve verbatim
    instructions: Option<String>,           // custom summarization prompt
}

impl CompactionConfig {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        model_spec: String,
        threshold: u64,
        keep_recent: usize,
        instructions: Option<String>,
    ) -> Self {
        Self { backend, model_spec, threshold, keep_recent, instructions }
    }

    /// Token threshold that triggers compaction. Read by the caller
    /// (`StandardConversation`) to decide when to invoke [`compact`].
    pub(crate) fn threshold(&self) -> u64 { self.threshold }

    /// User+assistant pairs to preserve verbatim at the tail of history.
    /// Read by the caller to compute the cut point.
    pub(crate) fn keep_recent(&self) -> usize { self.keep_recent }

    /// Run compaction on `messages`: try server-side, fall back to client-side.
    /// Returns the active signed block (if server-side succeeded) and a
    /// replacement message list. The replacement list starts with either the
    /// signed compaction block or a plain client-side summary message, followed
    /// by `kept` (the tail of the original history the caller wants to preserve
    /// verbatim — it is not re-compacted, just appended).
    pub(crate) async fn compact(
        &self,
        to_compact: &[Message],
        kept: &[Message],
        system_prompt: Option<&str>,
    ) -> Option<(Option<CompactionBlock>, Vec<Message>)> {
        // 1. Try server-side — returns a signed block that must be round-tripped.
        let server_block = self.compact_server_side(to_compact, system_prompt).await;

        // 2. Fall back to client-side — returns a plain assistant text message.
        //    Never fabricate a signed compaction block: an empty/forged
        //    signature would fail signature validation on every later request.
        let summary_msg = if server_block.is_some() {
            None
        } else {
            self.compact_client_side(to_compact).await
        };

        // 3. Build replacement history: compaction block (or client summary)
        //    first, then the kept messages.
        let first = match (&server_block, &summary_msg) {
            (Some(b), _) => compaction_block_as_message(b),
            (None, Some(m)) => m.clone(),
            (None, None) => return None,
        };
        let mut new_history = Vec::with_capacity(1 + kept.len());
        new_history.push(first);
        new_history.extend_from_slice(kept);
        Some((server_block, new_history))
    }

    async fn compact_server_side(
        &self,
        messages: &[Message],
        system_prompt: Option<&str>,
    ) -> Option<CompactionBlock> {
        match self.backend.compact(messages, system_prompt, self.instructions.as_deref()).await {
            Ok(Some(block)) => Some(block),
            _ => None,
        }
    }

    async fn compact_client_side(&self, messages: &[Message]) -> Option<Message> {
        let model = self.backend.make_model(&self.model_spec)?;
        let prompt = self.instructions.clone()
            .unwrap_or_else(|| DEFAULT_COMPACTION_PROMPT.into());

        // Single-turn completion: system = prompt, messages = the history.
        // Render the returned summary as a plain assistant text message — do
        // NOT construct a CompactionBlock, which requires a server signature.
        // The caller records this attempt and does not retry every turn (see
        // the "failure backoff" note below).
        // ...
        None // placeholder
    }
}
```

#### Failure backoff

Compaction is best-effort, but a persistently failing backend must not add
up to two full-history API calls to every turn. `StandardConversation` tracks
the last failed attempt (`last_compaction_attempt: Option<CompactionAttempt>`
keyed by the history length / token count at the time of the attempt) and
suppresses retries until enough new material has accumulated — e.g. `N` new
turns or `M` additional input tokens. This way a failing server-side call
doesn't also trigger the client-side fallback (and vice versa) on every
single turn.

### 6. `StandardConversation` (`conversation/standard.rs`)

#### New fields

```rust
pub(crate) struct StandardConversation {
    // ... existing fields (history, next_prompt, rendered_prompt, ...) ...

    compaction: Option<CompactionConfig>,   // None → compaction disabled
    compaction_block: Option<CompactionBlock>,  // active *signed* block, if any
    compaction_applied: bool,               // true once any summary has been swapped in
    last_compaction_attempt: Option<CompactionAttempt>, // failure-backoff state
}
```

#### Constructor

```rust
pub(crate) fn new(
    prompt: &str,
    preserve_reasoning: bool,
    prepopulated_messages: Option<Vec<Message>>,
    compaction: Option<CompactionConfig>,
) -> Self { ... }
```

`clone_box` and `fork`: `.clone()` the `CompactionConfig`, the
compaction block, `compaction_applied`, and `last_compaction_attempt`
(the `Arc<dyn Backend>` inside is cheap to clone).

#### `should_compact()`

Policy lives on `StandardConversation`. It reads threshold and keep-recent
from `CompactionConfig` as public accessors, combines them with conversation
state (history, active block, token estimate), and decides whether to trigger.

```rust
fn should_compact(&self) -> bool {
    let comp = match &self.compaction {
        Some(c) => c,
        None => return false,
    };
    if self.last_input_tokens() <= comp.threshold() {
        return false;
    }
    // After a summary (signed block or client-side text) has been swapped in,
    // don't re-compact until there is genuinely new material after it.
    if self.compaction_applied {
        let cut = compaction_cut_index(&self.history, comp.keep_recent());
        if cut == 0 {
            return false;
        }
    }
    true
}
```

#### Turn-boundary cut point

`keep_recent` is a count of *whole user/assistant turns*, not raw messages.
Agent history interleaves assistant `tool_use` and user `tool_result`
messages, so `history.len() - 2*keep_recent` can split a tool call from its
result (leaving `to_compact` ending in an unanswered tool call, which the API
rejects, and `kept` starting with an orphan `tool_result`). Compute the cut by
walking backwards over completed turns:

```rust
/// Compute a cut index so `to_compact = &history[..cut]` and
/// `kept = &history[cut..]`. The cut lands on a completed-turn boundary:
/// `keep_recent` whole user/assistant exchanges are kept verbatim and
/// `to_compact` never ends in an unanswered tool call.
fn compaction_cut_index(history: &[Message], keep_recent: usize) -> usize {
    let mut cut = history.len();
    let mut kept_turns = 0;
    while cut > 0 && kept_turns < keep_recent {
        // A run of user tool_result messages belongs to the preceding turn;
        // skip them before counting the boundary.
        while cut > 0 && is_user_tool_result(&history[cut - 1]) {
            cut -= 1;
        }
        // Step back over this turn's user opener and its assistant/tool
        // messages to the previous completed-turn boundary.
        cut = previous_turn_boundary(history, cut);
        kept_turns += 1;
    }
    cut
}
```

#### `render_turn` — compaction trigger

```rust
async fn render_turn(
    &mut self,
    nested: bool,
    scratchpad: Option<String>,
    system_prompt: Option<String>,
) -> TurnScript {
    // Phase 0: compaction (main loop only; not in nested/fork contexts)
    if !nested && self.should_compact() {
        if let Some(ref comp) = self.compaction {
            let cut = compaction_cut_index(&self.history, comp.keep_recent());
            let kept = self.history[cut..].to_vec();
            let to_compact = self.history[..cut].to_vec();
            if let Some((signed_block, new_history)) = comp.compact(&to_compact, &kept, system_prompt.as_deref()).await {
                self.history = new_history;
                self.compaction_applied = true;
                // `None` after a client-side fallback summary.
                self.compaction_block = signed_block;
            }
        }
    }

    // Phases 1-3: existing scratchpad/findings/prefix logic, unchanged
    // ...
}
```

#### `last_input_tokens()`

Lives on `StandardConversation` — token counting is a conversation concern.

The agent loop already receives `input_tokens` in each API response
(`StreamAccumulator.turn_usage`). Currently `record_turn` doesn't accept it.
Add an `input_tokens: Option<u64>` parameter so the conversation stores the
last known token count directly:

```rust
fn last_input_tokens(&self) -> u64 {
    self.last_input_tokens.unwrap_or(0)
}
```

No estimation needed — the API gives us the exact number. Compaction fires
when this crosses the threshold. We accept being one turn behind (the
current turn hasn't completed yet, so its tokens aren't counted), which is
fine for a best-effort optimization.

The change requires one new field on `StandardConversation`
(`last_input_tokens: Option<u64>`) and one extra argument at two call sites:

| Location | Change |
|---|---|
| `Conversation` trait, `record_turn` signature | Add `input_tokens: Option<u64>` |
| `agent_loop.rs` `commit_turn_to_history` | Pass `turn_usage.input_tokens` |
| `agent_loop.rs` `finish_stream_turn` | Already carries it through `StreamTurnOutput` |

#### Block storage

Only a server-issued *signed* compaction block is stored in `history` as an
assistant message with a compaction content block. This ensures:
- `messages()` returns it in the transcript
- `conversation.json` persistence includes it
- `clone_box` / `fork` propagate it
- The agent loop sends it to the model on every subsequent turn (with the
  beta header — see section 7)

```rust
fn compaction_block_as_message(block: &CompactionBlock) -> Message {
    Message::assistant(vec![
        AssistantContent::other(serde_json::json!({
            "type": "compaction",
            "content": block.content,
            "signature": block.signature,
        }))
    ])
}
```

If rig's `Message` serialization doesn't pass through unknown content block
variants, the fallback is to store the block separately on `StandardConversation`
and have `messages()` prepend it.

#### Compaction is best-effort

If both server-side and client-side compaction fail (network error, rate limit,
model error), the conversation proceeds with full history. Compaction is a
size/quality optimization, never a correctness requirement. Failures are
logged but never propagated as errors.

### 7. Agent loop (`agent_loop.rs`)

Two `.await` additions:

```rust
// prepare_stream_request becomes async
async fn prepare_stream_request(&mut self, ...) -> CompletionRequest {
    let script = self.conv.render_turn(
        nested,
        self.hooks.read_scratchpad(),
        self.system_prompt.clone(),
    ).await;  // ← .await added

    // ... rest unchanged
}
```

```rust
// Call site in stream_turn
let builder = self.prepare_stream_request(tool_defs, opts, nested).await; // ← .await
```

Whenever a signed compaction block is present in the conversation, the
ordinary completion path must also attach the `compact-2026-09-04` beta
header (see the API rules above: the beta header must accompany every
request carrying the block). The compaction block is stored in `history`, so
the agent loop is the component that adds this header. Concretely, the
completion request builder checks `self.conv.has_signed_compaction_block()`
and, when true, adds the beta header and applies the `cache_control`
breakpoint from the caching section. No other changes to the agent loop are
required.

### 8. Injection (`run.rs`)

```rust
// line 388 — client already resolved
let client = resolve_client(node, gremlin, enclosing_model)?;

// ... lines 389-536 ...

// Build compaction plumbing (new block, before StandardConversation::new)
let compaction: Option<CompactionConfig> =
    if let Some(cfg) = &prepared.compaction_config {
        let backend = client.get_or_build_backend()
            .map_err(|e| stage_failed(&agent.name, e))?;
        let spec = format!("{}/{}", client.uri.provider(), client.model());
        Some(CompactionConfig::new(
            backend,
            spec,
            cfg.threshold,
            cfg.keep_recent,
            cfg.instructions.clone(),
        ))
    } else {
        None
    };

// Inject
let conv = Box::new(StandardConversation::new(
    "", false, verbatim_history, compaction,
));
let prepopulated_conversation: Box<dyn Conversation> = conv;
```

No `set_backend` call — `Conversation` never sees a backend.

### 9. YAML configuration

Configuration flows through `extra_params` in the stage definition's `model`
block:

```yaml
stages:
  chat:
    type: agent
    model:
      provider: anthropic
      model: claude-sonnet-4-6
      compaction:
        threshold: 100000      # input tokens; omit to disable
        keep-recent: 3         # user+assistant pairs preserved verbatim
        instructions: |        # optional custom summarization prompt
          Summarize this design conversation. Preserve:
          - Entity names and field decisions
          - Rejected approaches and why they were rejected
          - The user's latest open request
          Do not call tools.
```

Path from YAML to `StandardConversation`:
1. `extra_params.compaction` parsed in `prepare_agent` → `AgentPrepared.compaction_config: Option<CompactionConfig>`
2. `run.rs` reads `prepared.compaction_config` → builds `StandardConversation`

---

## What doesn't change

| Component | Changes |
|---|---|
| `ConversationStub` (test) | `render_turn` must become `async`; `record_turn` gains `input_tokens` param (ignored by stub); `has_signed_compaction_block` returns `false` |
| `Conversation::fork` / `spawn_child` | `.clone()` the `CompactionConfig`, compaction block, `compaction_applied`, and `last_compaction_attempt` (the `Arc<dyn Backend>` inside is cheap) |
| `conversation.json` persistence | Compaction block stored in `history` → serializes normally |
| Non-Anthropic backends | Default `compact()` returns `None` → conversation falls through to client-side |
| `Task`/`Implement` fork children | `nested=true` in `render_turn` → compaction skipped |
| `cmd` / `exec` backends | Never see `compact()` — they don't use `StandardConversation` |

---

## Open questions

1. **CompactionBlock as a rig `Message`**: Does rig's `Message` serialization
   (serde) pass through unknown content block variants like
   `{"type": "compaction", ...}`? If not, `StandardConversation` needs a
   separate `compaction_block` field and `messages()` must prepend it.

2. **Client-side compaction 4xx errors on Anthropic backend**: How do we handle
   the case where the client-side compaction uses the Anthropic backend but the
   server blocks it due to missing/incorrect API keys?

3. **Token estimation precision**: Resolved — use the last response's
   `usage.input_tokens` directly, with no estimation. The API gives us the
   exact count. We accept being one turn behind (the current turn hasn't
   completed so its tokens aren't counted yet), which is fine for a
   best-effort optimization. Requires adding an `input_tokens` parameter to
   `Conversation::record_turn` and plumbing it from
   `commit_turn_to_history` in the agent loop.

4. **Compaction on fork children**: Currently skipped (`nested=true`). Fork
   children have short-lived conversations; compaction is unlikely to trigger.
   Revisit if long-running fork tasks become common.

5. **Multiple compactions in one conversation**: After compaction, the block
   is first in history. A subsequent compaction request summarizes the block
   + new messages into a new block. Only the newest block is sent. The API
   supports this ("compact again" in docs).