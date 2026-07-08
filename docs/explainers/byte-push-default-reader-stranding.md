# How a byte read is served, and when a push-model default read strands

This explains how the byte stream delivers data to a reader, and a known,
narrowly-scoped gap: a **default** reader can strand (its `read()` never
completes) when paired with a **push-model** source — one whose `pull()`
produces nothing and that enqueues later, out of band, from another task.

The pull gate itself is correct (see the busy-loop fix recorded in
`WPT_COVERAGE.md`). The stranding is a *separate* missing edge — enqueue does
not wake the byte task to serve reads parked inside it. This document maps the
machinery so the gap, and its blast radius, are legible.

## The picture

A byte stream has a **buffer** (the shelf), the **byte task** (the clerk — the
only thing that runs `pull()` and the only thing that serves *default* readers),
and **two wakers** ("doorbells").

```
                    ┌──────────── shared ByteStreamState ─────┐
   SOURCE ─enqueue─►│  buffer:      [ ][ ][ ]                 │
  (produces)        │  read_wakers  = doorbell A              │
                    │  pull_waker   = doorbell B              │
                    └──▲──────────────▲──────────────────▲────┘
        enqueue rings ─┘   polls ─────┘   rings B ────────┘
        A (only A!)        buffer,        "go pull"
                           registered A
              ┌──────────────┐        ┌──────────────────────────┐
              │ BYOB READER  │        │  BYTE TASK (the clerk)    │
              │ poll_read_   │        │  loop select!{pull, cmd}  │
              │ into: drains │        │  pending_reads = [ … ]  ◄─┼─ default
              │ buffer itself│        └──────────────────────────┘  readers'
              └──────────────┘        ▲   parked completions live    tickets
                                      │ StreamCommand::Read (channel) here
              ┌──────────────┐        │
              │DEFAULT READER│────────┘   woken ONLY by the task's
              │ sends Read,  │            oneshot completion
              │ awaits oneshot│
              └──────────────┘
```

Metaphor → code, so nothing here is hand-wavy:

| Metaphor | Real thing |
| --- | --- |
| shelf | `ByteStreamState.buffer` (`VecDeque<Bytes>`) |
| clerk | the byte task — `readable_byte_stream_task`'s `select!` loop |
| doorbell A | `read_wakers`, woken by `wake_readers()` |
| doorbell B | `pull_waker`, woken by `maybe_trigger_pull` / `force_pull` |
| waiting list | the task-local `pending_reads: VecDeque<oneshot::Sender<…>>` |

## The asymmetry that is the whole story

**The two doorbells, and who rings them:**

- **Doorbell A (`read_wakers`)** — rung by `enqueue_bytes` (via `wake_readers()`).
  Wakes anyone polling the buffer *directly* → BYOB / `AsyncRead`.
- **Doorbell B (`pull_waker`)** — rings the byte task. **`enqueue_bytes` does not
  ring it.**

**The two ways to pick up data (two "mailboxes"):**

- A **BYOB reader** polls the shared state directly (`poll_read_into`), which
  registers a `read_waker`. An enqueue rings A, it wakes and drains the buffer
  itself.
- A **default reader** sends a `StreamCommand::Read` to the task and awaits a
  **oneshot**. When the buffer is empty the task parks that oneshot sender in its
  own **`pending_reads`**. The reader is woken *only* when the task completes that
  oneshot.

And the one rule that ties it together: **the task drains `pending_reads` only
right after a `pull()` that grew the buffer** (or on a terminal close/error/cancel
that resolves them to EOF/error). Nothing else pokes that list.

## The healthy path — a pull-model source

Here the source produces *inside* `pull()`.

1. Default reader sends `Read`. Task: buffer empty → parks the completion in
   `pending_reads` → arms the gate (`maybe_trigger_pull`).
2. Task runs `pull()`. The pull enqueues into the buffer *right then* (the source
   produces synchronously during its own pull).
3. Pull returns; the task sees the buffer grew → drains `pending_reads` from the
   buffer → the read completes. ✅

It works because the data landed **while the task was already handling the pull**,
so the "buffer grew during the pull" drain fires immediately. That is the only
path that empties `pending_reads` during normal operation.

## The gap — a push-model source + default reader

Here `pull()` is a no-op and data arrives **later**, from a separate task calling
`enqueue` on a captured controller clone (a legitimate pattern — the byte
controller's `enqueue`/`close`/`error` take `&self` precisely so a captured clone
can drive the stream).

| # | actor | effect |
| --- | --- | --- |
| 1 | consumer | `reader.read()` → sends `Read`, awaits oneshot `rx` |
| 2 | byte task | buffer empty → parks sender in `pending_reads`; arms the gate |
| 3 | byte task | pull fires → no-op `pull()` → no growth → `mark_pull_completed(false)` → **task parks** |
| 4 | producer task | `controller.enqueue("pushed")` → buffer grows; `wake_readers()` rings doorbell A (nobody polling the buffer); **doorbell B untouched, task stays asleep** |
| 5 | — | data sits in buffer, sender sits in `pending_reads`, `rx` is never completed → **read hangs forever** |

The bug in one line: **enqueue rings the buffer-watcher's doorbell (A) but never
tells the task (B) to check `pending_reads` — so a default read parked on that
list sleeps forever even though its data is already on the buffer.**

That single missing edge — **enqueue → doorbell B → task drains `pending_reads`
from the current buffer** — is the entire problem. Steps 1–3 are all normal and
correct; parking in step 3 is the *right* behavior (efficient waiting). Step 4 is
the fault (the alarm clock is disconnected), and step 5 is only its symptom.

## Blast radius

| Scenario | Result | Why |
| --- | --- | --- |
| default reader, **read then push** | **strands** | parked in `pending_reads`; enqueue rings A, not B |
| default reader, **push then read** | served | buffer is non-empty when the `Read` is handled; served from the cmd branch |
| BYOB / `AsyncRead` reader, read then push | served | polls the buffer directly; enqueue rings A, which wakes it |
| pull-model source (any reader) | served | the source enqueues *inside* `pull()`, so growth is in-window |
| tee branch (push from coordinator) | served | the branch's `pull()` blocks on a `served` handshake until the coordinator has enqueued, dragging the enqueue *into* the pull window |

Two things to read off this:

- **It is ordering-dependent, and the bad ordering is the common one.** For a
  push source — where the consumer *waits* for data — the read arrives first and
  parks, so stranding is the normal path for that pattern, not a rare race.
- **It is not caused by the pull-gate fix.** The pre-fix busy-loop code strands
  the same case: once the push fills the buffer to/above the high-water mark, the
  old level-triggered gate stops re-arming too, and the parked read is never
  served — it just burned a core first. The fix changed a spin into a clean park;
  it neither introduced nor removed this gap.

## Why this is a missing edge, not the pull gate

Two different questions, asked by two different pieces of code:

- `mark_pull_completed` (the gate) answers **"should the task pull *again*?"**
- The missing edge would answer **"should the task *wake up* to hand out data?"**

Changing the gate cannot fix a wakeup that does not exist — which is why both the
level-triggered and the progress-gated gate strand identically. Different wire.

## The fix shape (deferred)

Close the edge: have `enqueue_bytes` wake the byte task (ring doorbell B, or a
dedicated serve signal), and have the task, on that wake, drain `pending_reads`
from the *current* buffer — not only after a pull that grew it. This is the same
restructuring that would let the tee's byte branch drop its `served` handshake and
use the default branch's pure-signal `pull()`, since the handshake exists only to
force the coordinator's push into the pull window.

It is deferred because the byte task and its read-serving path are central to
every byte stream, and the affected pattern (push-model source + default reader +
read-arrives-first) is narrow and has BYOB/`AsyncRead` as a working alternative.
The stranding is pinned by an `#[ignore]`d regression test
(`push_model_default_reader_strands`) so it cannot be silently forgotten.
