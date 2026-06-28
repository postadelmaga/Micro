<div align="center">

# ◇ framelite

**The minimal, generic core of the [Frame](../Frame) architecture — 100% Rust.**

A tiny modules + bus micro-kernel: a string-named pub/sub bus, a generic undoable
document, and an in-process module runtime. Nothing else. Build your app on top.

</div>

---

## What is framelite?

framelite is **Frame distilled to its skeleton**. Frame is a full desktop-app framework
(Dioxus/Vello UI, a rend3 3D sidecar, a `cpal` synth, sandboxed plugins). framelite keeps
only the part that makes that architecture *an architecture* — and makes it **generic**:

| | Frame | framelite |
|---|---|---|
| Channels | fixed enum (`Audio`/`Scene`/`Control`/`Input`) | **free-form strings** — app defines its own |
| Bus | LocalBus + IpcBus + shmem ring | **LocalBus only** (in-process pub/sub) |
| Hosting | in-process + supervised sidecars + remote | **in-process only** |
| Document | `Doc<TSchema>` + migrations | `Doc<S, A>` + reducer + undo/redo |
| Domain | scene, audio, kernels, trees, plugins | **none** — you bring it |

The boundaries are the same; the weight is gone. An app built *on* framelite adds renderers,
audio, sidecars, or plugins itself.

## The three ideas

1. **`Channel` + `Envelope` (`framelite-protocol`)** — the only types modules share. Routing
   is by channel name; a module never inspects another's internals.
2. **`LocalBus` (`framelite-bus`)** — an in-process pub/sub broker. Subscribe to channels,
   publish envelopes. Channels marked `retain` keep their last value and replay it to late
   subscribers (the generic form of Frame's "replay a State channel on resubscribe").
3. **`Doc<S, A>` + reducer (`framelite-document`)** — the single source of truth. Every edit
   is a **serializable action** `A` applied by one reducer `Fn(&mut S, &A)`, transactionally
   (on a clone first) and undoably. Mutations are *data*: loggable, replayable, bus-sendable.

`framelite-core` ties them together: a **`Module`** declares its channel subscriptions and a
`run` loop; the **`Runtime`** subscribes it to the bus and spawns it on its own thread.
Modules talk only through their `ModuleCtx` — never to each other — so any module is
swappable.

## Robustness

framelite is small but not fragile — the bus and runtime are bounded and supervised:

- **Bounded inboxes, no OOM.** Every subscriber queue is bounded (`sync_channel`); a runaway
  producer can't grow memory without limit.
- **Per-channel backpressure.** `bus.set_overflow(ch, …)`: `Overflow::Drop` (default) never
  blocks and counts losses in `bus.dropped()`; `Overflow::Block` gives true source-slowing
  backpressure (the producer is paced by the slowest consumer, nothing lost).
- **First-class lifecycle.** A cooperative `Shutdown` signal (`ctx.should_stop()`,
  `Runtime::shutdown()`), `recv_timeout` so loops wake to observe it, and **fail-fast
  supervision**: a module panic is isolated to its thread, reported by `Runtime::join()`, and
  trips a clean shutdown of the rest.
- **No head-of-line blocking.** A shared worker pool lets a module `ctx.offload(job)` heavy CPU
  work off its receive loop — *receive fast, compute on the pool, publish the result back*.

## Layout

```
crates/
  framelite-protocol/  ModuleId, Channel, Envelope, ChannelKind   (zero logic)
  framelite-bus/       Sender/Receiver traits + LocalBus broker (+ retained channels)
  framelite-document/  History<S>, Doc<S, A> + reducer          (undo/redo, transactional)
  framelite-core/      Module, ModuleCtx, Runtime + worker pool (the in-process micro-kernel)
```

**Enforced boundaries:** `protocol` depends on nothing; `bus` and `document` depend only on
`protocol` (and `document` not even that); only `core` composes everything.

## Try it

```sh
cargo test --workspace
cargo run -p framelite-core --example counter
```

The `counter` example wires a `Ticker` and a `Store` over the bus: the ticker emits
increments on `tick`, the store applies them to a `Doc<i64, _>` and republishes the running
total on the retained `count` channel — two modules that never reference each other.

```
count = 1
count = 2
count = 3
count = 4
count = 5
done.
```

## Extending it

- **A new module** = implement `Module`, name its channels, write its `run` loop, `rt.spawn(it)`.
- **A new edit** = add a variant to your action enum, handle it in the reducer, `doc.dispatch(&action)`.
- **Heavy work** = `ctx.offload(job)` so the receive loop keeps draining.
- **A new topic** = just publish on a new channel name. No core changes.
- **A new transport** (sidecar, socket) = implement the `Sender`/`Receiver` traits; modules
  written against them don't change. (Not shipped — this is the seam Frame grows along.)

---

<sub>Built with Rust 🦀 — the minimal core distilled from Frame.</sub>
