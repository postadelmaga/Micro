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
| Document | `Doc<TSchema>` + migrations | `Doc<S>` + undo/redo |
| Domain | scene, audio, kernels, trees, plugins | **none** — you bring it |

The boundaries are the same; the weight is gone. An app built *on* framelite adds renderers,
audio, sidecars, or plugins itself.

## The three ideas

1. **`Channel` + `Envelope` (`framelite-protocol`)** — the only types modules share. Routing
   is by channel name; a module never inspects another's internals.
2. **`LocalBus` (`framelite-bus`)** — an in-process pub/sub broker. Subscribe to channels,
   publish envelopes. Channels marked `retain` keep their last value and replay it to late
   subscribers (the generic form of Frame's "replay a State channel on resubscribe").
3. **`Doc<S>` + `Command<S>` (`framelite-document`)** — the single source of truth. Every
   edit is a named, fallible, undoable command applied transactionally (on a clone first).

`framelite-core` ties them together: a **`Module`** declares its channel subscriptions and a
`run` loop; the **`Runtime`** subscribes it to the bus and spawns it on its own thread.
Modules talk only through their `ModuleCtx` — never to each other — so any module is
swappable.

## Layout

```
crates/
  framelite-protocol/  ModuleId, Channel, Envelope, ChannelKind   (zero logic)
  framelite-bus/       Sender/Receiver traits + LocalBus broker (+ retained channels)
  framelite-document/  History<S>, Command<S>, Doc<S>            (undo/redo, transactional)
  framelite-core/      Module, ModuleCtx, Runtime               (the in-process micro-kernel)
```

**Enforced boundaries:** `protocol` depends on nothing; `bus` and `document` depend only on
`protocol` (and `document` not even that); only `core` composes everything.

## Try it

```sh
cargo test --workspace
cargo run -p framelite-core --example counter
```

The `counter` example wires a `Ticker` and a `Store` over the bus: the ticker emits
increments on `tick`, the store applies them to a `Doc<i64>` and republishes the running
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
- **A new edit** = implement `Command<S>` and `doc.dispatch(&cmd)`.
- **A new topic** = just publish on a new channel name. No core changes.
- **A new transport** (sidecar, socket) = implement the `Sender`/`Receiver` traits; modules
  written against them don't change. (Not shipped — this is the seam Frame grows along.)

---

<sub>Built with Rust 🦀 — the minimal core distilled from Frame.</sub>
