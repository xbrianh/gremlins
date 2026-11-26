# gremlins-magic-bus — agent orientation

`gremlins-magic-bus` is a domain-agnostic fan-out event registry. It routes
`DispatchEvent<P>` values to subscribers based on segment-aware `CorrId` prefix
matching, using the delivery primitive implied by each subscriber's
`DeliveryPolicy`.

The gremlins layer instantiates it as `Bus<EventKind>` in
`concrete.rs` and exposes it through `emit_event(EventKind)` / `subscribe_all()`.
The bus core stays unaware of any payload beyond the concrete `EventKind`
(`event_kind.rs`), which lives here so the executor and client layers can share
it without depending on each other.

## Design principles

These principles govern every use of the bus in the gremlins codebase:

1. **The bus is a global singleton.** One `Bus<EventKind>` per process,
   initialized once in `concrete.rs::init_bus()`. There is never a second
   instance and the bus is never reset. Callers reference it through the
   free functions in `concrete.rs` (`emit_event`, `subscribe_all`, etc.),
   never by holding a reference to the `Bus` itself.

2. **Messages are sent at their point of origin.** No aggregation on the
   sender side. No closures, no re-emit, no message handling logic. The
   only infrastructure a sender needs is the correlation id prefix
   (`CorrId`) to tag events with their run identity:

   ```rust
   // In the agent loop, when a tool result arrives:
   emit_run_event_async(&state.message_prefix, EventKind::ToolResult { … }).await;

   // In stream_consume, when a text delta arrives:
   emit_run_event_async(&state.message_prefix, EventKind::StreamChunk { … }).await;

   // In turn_flow, when a turn completes:
   emit_run_event_async(&state.message_prefix, EventKind::TurnComplete { … }).await;
   ```

   Each `emit` call is a single fire-and-forget (or fire-and-await)
   operation at the exact point where the event naturally occurs. The
   sender never collects events into a buffer for later delivery, never
   wraps emit calls in a helper that batches them, and never inspects the
   event after emitting it.

3. **Consumer-specific aggregation is performed on the receiver side.** Subscribers
   join, filter, buffer, and fold events according to their own needs.
   The sender should never batch, debounce, or collect events for a
   specific consumer — that is the subscriber's responsibility.

   Internal event consumers subscribe and drain events independently of
   viewer watches. `subscribe_run_backpressured` provides bounded delivery
   for internal consumers; it is not a chat-supervisor front-end forwarding
   contract. Each viewer watch uses independent lag-tolerant delivery: a
   stalled viewer lags rather than backpressuring the producer. Front-end
   delivery is not guaranteed lossless. The agent loop has no knowledge
   that a viewer exists.

These principles drove the streaming refactor in #2273 (the
`gremlins-magic-bus` crate extraction). Before the refactor, the agent
loop accumulated events into closures that were passed down to the chat
layer — aggregation on the sender side. After the refactor, every event
is emitted at its origin and the chat layer subscribes independently.

## Module layout

| File | Contents |
|---|---|
| `lib.rs` | Module declarations and the crate's public re-exports |
| `bus.rs` | `Bus<P>` — in-memory run journal, atomic `subscribe_with_history`, fan-out registry |
| `concrete.rs` | The global `Bus<EventKind>`: `init_bus`, `subscribe_all`, `subscribe_run`, `subscribe_run_lag_tolerant`, `subscribe_run_backpressured`, `emit_event`, `emit_run_event_async` |
| `connection.rs` | `SharedWriter`, `BusSubGuard`, `ConnectionGuard` |
| `corr_id.rs` | `CorrId` — segment-aware correlation id, prefix matching, tagged string form + parser |
| `event.rs` | `DispatchEvent<P>` and `Timestamp` — the typed envelope |
| `event_kind.rs` | `EventKind` — the concrete payload the gremlins layer publishes |
| `policy.rs` | `DeliveryPolicy` and `SubscriberRegistration` — delivery semantics |
| `receiver.rs` | `ReceiverHalf<P>`, `Recv<P>`, `TryRecvError` — the half a subscriber polls |
| `stats.rs` | `UsageStats`, `TurnStats`, `add_opt` — token accounting |
| `subscriber.rs` | `Subscriber<P>` handle and `SubscriberMailbox<P>` — producer side + channel construction |

Public surface (re-exported from `lib.rs`): `Bus`, `CorrId`, `ParseCorrIdError`,
`DispatchEvent`, `HistorySnapshot`, `Timestamp`, `EventKind`, `DeliveryPolicy`,
`SubscriberRegistration`, `ReceiverHalf`, `Recv`, `TryRecvError`, `Subscriber`,
`SubscriberError`, `BusSubGuard`, `ConnectionGuard`,
`SharedWriter`, `UsageStats`, `TurnStats`, `add_opt`, the concrete emitter
functions. Unix-domain socket and daemon-lock helpers live in
`gremlins::executor::socket`, not this crate.

## Key concepts

### Correlation ids

A `CorrId` names a point in the gremlin lifecycle:

```text
run -> stage -> turn -> tool -> attempt
```

- Every field is optional; the empty `CorrId::new()` matches everything.
- Prefix matching is **segment-aware, not string-based**: `r:ab` must not match
  `r:abc`. A `None` prefix field is a wildcard for that segment only.
- Run filters retain colon-component ancestor matching: run `parent` matches
  `parent:child`, but not `parentish`. Other fields match exactly.
- `with_stage_run_id(impl Into<String>)` sets an optional opaque stage-run id;
  `stage_run_id() -> Option<&str>` borrows it. Colons have no hierarchy meaning
  in stage-run ids. An omitted stage-run filter is a wildcard, including for
  legacy events without a stage-run id. A set filter requires an exact value.
  This field is routing metadata only; the bus does not attribute events at
  registration or dequeue time.
- The tagged string form is `r:<run>:s:<stage>:t:<turn>:c:<tool>:a:<attempt>:x:<stage_run_id>`,
  in that fixed order. Legacy forms without `x` are unchanged,
  with missing segments omitted. String values may contain `:` and `\`, escaped
  as `\:` and `\\` on render, so `to_string().parse::<CorrId>()` always
  round-trips. String segments must be non-empty.
- `with_run`/`with_stage`/`with_tool`/`with_stage_run_id` panic on empty values; `try_parse` rejects
  them with `ParseCorrIdError::EmptyValue`.

### Delivery policies

`DeliveryPolicy` selects the channel primitive backing a subscriber:

| Variant | Backing channel | Producer behavior |
|---|---|---|
| `Lossless` | unbounded `mpsc` | never blocks, never drops |
| `Backpressured { bound }` | bounded `mpsc` | `emit` drops when full; `emit_async` awaits capacity |
| `LagTolerant { capacity }` | `broadcast` | drops on `Lagged`; receiver resyncs from folded state |

Bounded and lag-tolerant channels require a non-zero capacity
(`SubscriberError::ZeroCapacity` otherwise).

### Emit vs emit_async

- Both emission paths serialize journal insertion and nonbackpressured mailbox
  enqueue under one state mutex. The in-memory journal has no disk persistence.
  `Bus::new()` defaults to **16,384 events per exact run** and **65,536 globally**.
  Oldest records are evicted to enforce both limits; only runs with retained
  records are tracked. Descendant runs have separate per-run budgets.
- `Bus::with_history_limits(per_run, global)` customizes these record limits
  (including zero). `Bus::with_history_limits_and_retention(per_run, global,
  predicate)` also accepts `predicate: Fn(&P) -> bool + Send + Sync + 'static`.
  The predicate runs before taking the state mutex; keep it cheap and nonblocking.
  Rejected payloads are omitted from history without changing live delivery.
  The concrete global bus excludes `EventKind::PromptRequest` from history but
  still delivers it live. This generic journal is not consumer-specific aggregation.
- `subscribe_with_history(registration)` takes that mutex to snapshot matching
  run history and register live delivery atomically. It returns
  `(sub, rx, HistorySnapshot<P>)`; history is separate from the live mailbox.
  Matching uses the same full prefix (including descendant runs, stage,
  stage-run id, etc.) as live routing. The snapshot is an atomic history/live
  cut, with original envelopes in emission order.
- `HistorySnapshot` has `events: Vec<DispatchEvent<P>>`, `complete: bool`,
  `through_sequence: u64`, and `evicted_through_sequence: u64`.
  The bus-global sequence advances for every run-scoped event, including omitted
  events; non-run events are live-only and do not advance it. `through_sequence`
  is the latest sequence at the cut (initially zero).
  `evicted_through_sequence` is the maximum sequence evicted or omitted anywhere
  in the bus (zero if none). After **any** global eviction, per-run eviction, or
  retention omission, `complete` is permanently, conservatively false, even for
  unrelated filters with no matching lost records.
- `emit_with_persistence(event, persist)` publishes and assigns the envelope first,
  then invokes `persist` **outside** the state mutex. Subscribers may observe the
  event before persistence; concurrent callbacks can finish out of sequence order,
  and a callback panic does not undo publication. This is not an atomic disk cut.
- `with_sequence_snapshot(read)` and concrete `with_run_replay_cut(read)` run the
  read outside the bus mutex and return a compatibility watermark of **zero**.
  They assert no persisted coverage and must not suppress positive live sequences.
  Callbacks may re-enter the bus or wait for emissions.
- Sequences are process-local, not durable identities across daemon restarts.
  Exact persisted stage-run-scoped agent history/live merging is deferred; the
  atomic in-memory history/live cut does not establish that guarantee.
- `subscribe_run_with_history(id, capacity)` exposes this with lag-tolerant live
  delivery. Overflow still reports lag. Bounded history introduces no new
  backpressure and does not change live delivery policies.
- `emit_async` delivers lossless/broadcast mailboxes under the mutex, first-polls
  bounded capacity reservations there, then awaits pending reservations concurrently
  outside the mutex. Slow bounded consumers cannot delay other policies.
- Reservations enter Tokio's FIFO capacity queue in journal order. Bounded
  delivery is not an exact journal-order contract: with multiple available permits,
  task scheduling can reorder the eventual sends. Cancellation drops pending bounded
  delivery, sync emits drop when capacity is unavailable, and direct subscriber
  sends bypass the bus. Lossless/broadcast bus delivery has exact journal order
  (broadcast may report skipped entries on overflow).
- Concrete emitters initialize the global bus, retaining early run events. If first
  initialization occurs outside Tokio, the optional log-forwarding task is skipped.

## Display and input boundary

- The bus carries display and lifecycle events, not execution input or controller
  ownership. `StageBoundary` identifies an invocation and its active lifetime;
  envelopes/watch frames retain descriptive `stage_type` metadata.
- `stage_type: "agent"` is not a writable capability or a liveness guarantee.
  Bus events do not advertise writable/debugger-ready attachment capabilities.
- Passive `watch` consumes this output. Exact input is resolved separately by
  `gremlins::executor::supervisor::input`, which claims the live execution inbox;
  neither bus subscription nor replay authorizes input.

## Invariants

- **No I/O or awaits under the state mutex.** Journal insertion, history snapshot
  cloning, subscription registration, routing, and nonblocking enqueue are atomic
  under that lock. Persistence/read callbacks and capacity waits happen outside;
  disk persistence is not atomic with publication.
- **Poison panics are intentional.** `.expect("bus state poisoned")` prevents
  silently continuing after a panic during journal/routing updates.
- **Segment matching is exact.** Never replace `matches_prefix` with
  `str::starts_with` — that reintroduces the `r:ab` matching `r:abc` bug the
  segment parser exists to prevent.
- **Round-trip is a contract.** `CorrId`'s display/parse pair must survive
  `:` and `\` in string segments. Any change to the tag set or escaping must be
  accompanied by round-trip tests.
- **`ConnectionGuard` is closure-based.** `new(up, down)` calls `up`
  immediately and stores a `Send + 'static` `FnOnce` hook to call on drop.
  It has no actor-message type parameter; callers capture registry handles
  in the hooks.

## Usage pattern

```rust
use gremlins_magic_bus::{
    Bus, CorrId, DispatchEvent, Recv, SubscriberRegistration,
};

let bus: Bus<MyPayload> = Bus::new();
let (subscriber, mut rx) = bus
    .subscribe(SubscriberRegistration::lossless(CorrId::new().with_run("X")))
    .expect("lossless subscription cannot fail");

bus.emit(DispatchEvent::new(
    CorrId::new().with_run("X").with_stage("a"),
    MyPayload,
));

match rx.recv().await {
    Recv::Event(dispatch) => { /* dispatch.corr_id(), dispatch.kind(), dispatch.ts() */ }
    Recv::Lagged(n) => { /* resync from folded state */ }
    Recv::Closed => { /* drain and exit */ }
}
```

A subscriber's receiver observes `Recv::Closed` after `unsubscribe` once
in-flight deliveries drain, so subscriber tasks can close their log blob or
connection and exit cleanly.

## How it's wired into gremlins

Global wiring lives in `concrete.rs`; the daemon uses it from
`crates/gremlins/src/executor/supervisor/mod.rs`:

- `init_bus()` — one-time construction plus a lag-tolerant subscriber that
  routes every `LogLine` event through the `log` crate, so call sites only need
  `emit_event(EventKind::LogLine { … })`.
- `emit_event(EventKind)` — maps run-bearing event kinds onto a run-scoped
  `CorrId::new().with_run(id)` so per-gremlin subscribers see only their own
  run; other kinds use the empty `CorrId` and reach only wildcard-compatible filters.
- `subscribe_all()` — the connection-forwarder subscription.

## Where to look

| You want to … | Look at |
|---|---|
| Add a new delivery semantics | `policy.rs` + `subscriber.rs::SubscriberMailbox` |
| Change how events are matched | `corr_id.rs::matches_prefix` |
| Add a field to a correlation id | `corr_id.rs` (FIELD_TAGS, struct, builder, parser, Display) |
| Add an event variant | `event_kind.rs::EventKind` |
| Change global bus wiring | `concrete.rs::init_bus`, `subscribe_all`, `emit_event` |
