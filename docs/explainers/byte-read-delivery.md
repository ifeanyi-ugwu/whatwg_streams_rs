# How a byte read is delivered

This explains how the byte stream gets data to a reader — the paths a chunk can
take from the source's `enqueue` to a waiting `read()` — and why a **default**
reader needs a wakeup that a **BYOB** reader does not.

The interesting case is a **push-model** source: one whose `pull()` produces
nothing and that `enqueue`s later, out of band, from another task (a captured
controller clone — the byte controller's `enqueue`/`close`/`error` take `&self`
precisely so a captured clone can drive the stream). That case exercises every
delivery path.

## The picture

A byte stream has a **buffer** (the shelf), the **byte task** (the clerk — the
only thing that runs `pull()` and the only thing that serves *default* readers),
and three "doorbells."

```
                    ┌──────────── shared ByteStreamState ─────┐
   SOURCE ─enqueue─►│  buffer:      [ ][ ][ ]                 │
  (produces)   │    │  read_wakers  = doorbell A              │
               │    │  serve gate   = doorbell B  (the task)  │
               │    │  pull gate    = doorbell C  (the task)  │
   enqueue rings└───┤                                         │
   A and B          └──▲──────────────▲──────────────────▲────┘
                       │ polls buffer  │ serve gate wakes │ pull gate
                       │ (note A)      │ the task         │ wakes the task
              ┌────────┴─────┐   ┌──────┴──────────────────┴──┐
              │ BYOB READER  │   │  BYTE TASK (the clerk)      │
              │ poll_read_   │   │  select!{ pull, serve, cmd }│
              │ into: drains │   │  pending_reads = [ … ]  ◄───┼─ default
              │ buffer itself│   │  drained by pull-grow OR    │  readers'
              └──────────────┘   │  the serve gate             │  tickets
                                 └─────────────────────────────┘
```

Metaphor → code, so nothing here is hand-wavy:

| Metaphor | Real thing |
| --- | --- |
| shelf | `ByteStreamState.buffer` (`VecDeque<Bytes>`) |
| clerk | the byte task — `readable_byte_stream_task`'s `select!` loop |
| doorbell A | `read_wakers`, woken by `wake_readers()` |
| doorbell B (serve gate) | `serve_pending` + `serve_waker`, via `notify_serve` / `poll_serve_needed` |
| doorbell C (pull gate) | `needs_pull` + `pull_waker`, via `maybe_trigger_pull` / `poll_pull_needed` |
| waiting list | the task-local `pending_reads: VecDeque<oneshot::Sender<…>>` |

## Two mailboxes

There are two ways a reader picks up data, and this asymmetry is the whole reason
default readers need doorbell B:

- A **BYOB / `AsyncRead`** reader polls the shared state directly
  (`poll_read_into`), which registers a `read_waker`. It watches the buffer; an
  enqueue rings doorbell A and it drains the buffer itself.
- A **default** reader sends a `StreamCommand::Read` to the task and awaits a
  **oneshot**. When the buffer is empty the task parks that oneshot sender in its
  own **`pending_reads`**. The reader is woken *only* when the task completes that
  oneshot — its "waiting ticket" lives *inside the task*, invisible to a bare
  buffer waker.

## Three delivery paths

**1. In-pull drain (pull-model source).** The source produces *inside* `pull()`.

1. Default reader sends `Read`; buffer empty → parked in `pending_reads`; the pull
   gate is armed.
2. The task runs `pull()`, which enqueues into the buffer right then.
3. The pull returns, the task sees the buffer grew, and drains `pending_reads`
   from the buffer. ✅

The data landed *while the task was handling the pull*, so the "buffer grew during
the pull" drain hands it out immediately.

**2. Serve gate (out-of-band enqueue / close / error).** The source produces
*outside* any pull — a push-model source enqueuing from another task, or an
out-of-band `close`/`error`.

1. Default reader sends `Read`; buffer empty → parked in `pending_reads`; the pull
   gate fires a no-op `pull()` that produces nothing, and the task parks.
2. Later, `enqueue("pushed")` runs on a captured controller: it appends to the
   buffer, rings doorbell A (for any BYOB watcher), **and rings doorbell B —
   `notify_serve` — waking the task's serve gate.**
3. The task's serve branch drains `pending_reads` from the current buffer, and the
   read completes. ✅ (`close`/`error` ring the serve gate too, so a parked read
   gets EOF / the error rather than hanging.)

Doorbell B is the wire that makes push-model default readers work: without it, an
out-of-band enqueue rings only doorbell A, which a default reader never
registered, so the parked read would sleep forever with its data on the shelf.

**3. Direct state poll (BYOB / `AsyncRead`).** The reader polls `poll_read_into`
and registers a `read_waker`. An enqueue rings doorbell A and the reader drains the
buffer itself — it never uses `pending_reads` or the serve gate.

## Why the serve gate is a separate doorbell from the pull gate

They answer different questions:

- The **pull gate** (`mark_pull_completed` / `needs_pull`) answers **"should the
  task pull *again*?"** — re-armed only by progress (an enqueue during a pull), so
  a progress-less `pull()` parks instead of busy-looping (see the pull-gate note in
  `WPT_COVERAGE.md`).
- The **serve gate** (`notify_serve` / `serve_pending`) answers **"should the task
  *wake up to hand out* already-buffered data?"** — rung by every out-of-band
  `enqueue`/`close`/`error`.

Folding the second into the first would be wrong: arming the pull gate on an
enqueue would fire a real `pull()` (a no-op for a push source) instead of just
serving the read. Two questions, two doorbells.

## Delivery matrix

| source | reader | path | result |
| --- | --- | --- | --- |
| pull-model | default | in-pull drain | ✅ |
| pull-model | BYOB / AsyncRead | direct state poll | ✅ |
| push-model | BYOB / AsyncRead | direct state poll (doorbell A) | ✅ |
| push-model | default, push-then-read | cmd branch (buffer already full) | ✅ |
| push-model | default, read-then-push | **serve gate (doorbell B)** | ✅ |

The last row is the one that mattered: read-before-push is the *normal* ordering
for a push source (the consumer waits for data), and it is served by the serve
gate. Regression tests: `push_model_default_reader_is_served` (data) and
`push_model_default_reader_gets_eof_on_out_of_band_close` (terminal).

## A note on the tee

A byte `tee` branch is fed by the coordinator, which enqueues from a separate task
— itself a push producer. The branch source's `pull()` blocks on a `served`
handshake until the coordinator has enqueued, which lands the enqueue inside the
branch's pull window (delivery path 1). With the serve gate now present, path 2
would deliver the branch's parked reads even without that handshake, so `served`
is no longer required for *correctness*; it is retained because the tee's exact
pull-count behaviour is verified against it, and reworking that is out of scope
here.
