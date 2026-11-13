# magic_bus — agent orientation

`magic_bus` is a domain-agnostic fan-out event registry. It knows nothing about
gremlin event kinds; it routes `DispatchEvent<P>` values to subscribers based on
segment-aware `CorrId` prefix matching, using the delivery primitive implied by
each subscriber's `DeliveryPolicy`.

The gremlins layer instantiates it as `Bus<EventKind>` in
`crates/gremlins/src/executor/supervisor.rs` and exposes it through
`emit_event(EventKind)` / `subscribe_all()`. The bus core should stay unaware of
that layer.

## Module layout

| File | Contents |
|---|---|
| `mod.rs` | `Bus<P>` — the fan-out registry, `subscribe` / `unsubscribe` / `emit` / `emit_async` |
| `corr_id.rs` | `CorrId` — segment-aware correlation id, prefix matching, tagged string form + parser |
| `event.rs` | `DispatchEvent<P>` and `Timestamp` — the typed envelope |
| `policy.rs` | `DeliveryPolicy` and `SubscriberRegistration` — delivery semantics |
| `receiver.rs` | `ReceiverHalf<P>`, `Recv<P>`, `TryRecvError` — the half a subscriber polls |
| `subscriber.rs` | `Subscriber<P>` handle and `SubscriberMailbox<P>` — producer side + channel construction |

Public surface (re-exported from `mod.rs`): `CorrId`, `ParseCorrIdError`,
`DispatchEvent`, `Timestamp`, `DeliveryPolicy`, `SubscriberRegistration`,
`ReceiverHalf`, `Recv`, `TryRecvError`, `Subscriber`, `SubscriberError`.
`SubscriberMailbox` is `pub(crate)` — it is an implementation detail.

## Key concepts

### Correlation ids

A `CorrId` names a point in the gremlin lifecycle:

```text
run -> stage -> turn -> tool -> attempt
```

- Every field is optional; the empty `CorrId::new()` matches everything.
- Prefix matching is **segment-aware, not string-based**: `r:ab` must not match
  `r:abc`. A `None` prefix field is a wildcard for that segment only.
- The tagged string form is `r:<run>:s:<stage>:t:<turn>:c:<tool>:a:<attempt>`,
  with missing segments omitted. String values may contain `:` and `\`, escaped
  as `\:` and `\\` on render, so `to_string().parse::<CorrId>()` always
  round-trips. String segments must be non-empty.
- `with_run`/`with_stage`/`with_tool` panic on empty values; `try_parse` rejects
  them with `ParseCorrIdError::EmptyValue`.

### Delivery policies

`DeliveryPolicy` selects the channel primitive backing a subscriber:

| Variant | Backing channel | Producer behavior | Analog in old code |
|---|---|---|---|
| `Lossless` | unbounded `mpsc` | never blocks, never drops | `log_tx` |
| `Backpressured { bound }` | bounded `mpsc` | `emit` drops when full; `emit_async` awaits capacity | chat stream subscriber |
| `LagTolerant { capacity }` | `broadcast` | drops on `Lagged`; receiver resyncs from folded state | `EVENT_BROADCAST` |

Bounded and lag-tolerant channels require a non-zero capacity
(`SubscriberError::ZeroCapacity` otherwise).

### Emit vs emit_async

- `Bus::emit` is synchronous and never blocks. It holds the registry read lock
  and does only `try_send`-style work under it. This is the path for
  log-shaped records.
- `Bus::emit_async` collects matching mailboxes under the read lock, **releases
  the lock before any `.await`**, and delivers concurrently via `join_all` so a
  slow backpressured subscriber cannot head-of-line block the others.

## Invariants

- **Domain agnosticism.** Do not introduce `EventKind` or any gremlin-specific
  payload type here. `P` stays generic; the executor layer owns the payload.
- **Don't block under the read lock.** Registration is rare, emission is hot.
  The registry is `RwLock<Vec<(CorrId, SubscriberMailbox<P>)>>`; keep anything
  that can wait (async sends, allocations in the hot path) out of the read
  guard.
- **Poison panics are intentional.** `.expect("subscriber registry poisoned")`
  is used deliberately: a poisoned registry means a subscriber panicked while
  holding the write lock, and silently continuing would corrupt routing.
- **Segment matching is exact.** Never replace `matches_prefix` with
  `str::starts_with` — that reintroduces the `r:ab` matching `r:abc` bug the
  segment parser exists to prevent.
- **Round-trip is a contract.** `CorrId`'s display/parse pair must survive
  `:` and `\` in string segments. Any change to the tag set or escaping must be
  accompanied by round-trip tests.

## Usage pattern

```rust
use crate::magic_bus::{
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

See `crates/gremlins/src/executor/supervisor.rs`:

- `static BUS: OnceLock<Bus<EventKind>>` — the process-wide bus.
- `init_bus()` — one-time construction plus a lag-tolerant subscriber that
  routes every `LogLine` event through the `log` crate, so call sites only need
  `emit_event(EventKind::LogLine { … })`.
- `emit_event(EventKind)` — maps run-bearing event kinds onto a run-scoped
  `CorrId::new().with_run(id)` so per-gremlin subscribers see only their own
  run; other kinds use the empty `CorrId` and reach everyone.
- `subscribe_all()` — the connection-forwarder subscription (replaces the old
  broadcast-channel subscribe).

## Where to look

| You want to … | Look at |
|---|---|
| Add a new delivery semantics | `policy.rs` + `subscriber.rs::SubscriberMailbox` |
| Change how events are matched | `corr_id.rs::matches_prefix` |
| Add a field to a correlation id | `corr_id.rs` (FIELD_TAGS, struct, builder, parser, Display) |
| Wire a new event through the bus | `executor/event.rs` + `executor/supervisor.rs` |
| See the payload type the executor uses | `executor/event.rs::EventKind` |
