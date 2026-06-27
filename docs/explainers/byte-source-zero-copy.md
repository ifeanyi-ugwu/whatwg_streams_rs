# Why a byte source has one pull method, not two

This explains the reasoning behind `ReadableByteSource`'s single, enqueue-driven
`pull`: why the WHATWG byte stream offers a source two ways to produce bytes, why
this library collapses them into one, and how the consumer side reads — the
queue-copy `read(&mut [u8])` and the zero-copy `read_owned`.

The decision is recorded in [ADR 0001](../adr/0001-zero-copy-byte-streams.md).
This document is the longer "why," meant for understanding rather than for
deciding.

## What both spec methods are answering

A byte source's whole job is to get bytes into the stream's queue without copying
them. The WHATWG spec gives a source two ways to do it:

- `controller.enqueue(view)` — the source allocated its own buffer, wrote bytes
  into it, and hands it over.
- A BYOB request + `respond(n)` (the `autoAllocateChunkSize` path) — the
  controller hands the source a buffer, the source fills it, and calls `respond`.

The question is why JavaScript needs both, and why Rust needs only the first.

## Why JavaScript needs two

It comes down to one limitation: JavaScript has no cheap way to transfer ownership
of memory. A buffer is shared, garbage-collected memory; anyone with a reference
can read or write it. To move bytes into the stream "without copying" while
staying safe — no two parties scribbling on the same buffer — the spec uses
**transfer/detach**, a runtime operation that marks the source's reference dead.

- Method 1 works when the source owns the buffer: it allocates, fills, enqueues,
  and the runtime detaches it.
- Method 2 exists for the case the language cannot otherwise express: writing
  directly into the *consumer's* final buffer. When a BYOB reader supplies a
  buffer `B`, the ideal is for the source to read straight into `B` — zero hops,
  source to consumer. JavaScript has no way to hand the source a writable view of
  `B` except through the controller's BYOB-request machinery.

Two methods, because there are two ownership situations, and JavaScript can only
bridge them through runtime detachment.

## Why Rust needs only one

Rust has the thing JavaScript lacks: ownership and `move` are first-class and
free.

"Transfer into the stream" is a `move`. `controller.enqueue(bytes)` moves the
buffer into the queue, and the borrow checker guarantees the source cannot touch
it afterward — no runtime "detached" flag, no copy. Because `move` is universal,
method 1 covers every source, including file and socket readers: allocate an
owned `Vec`/`BytesMut`, read into it, enqueue it. Zero copy into the queue.

So the only thing method 2 uniquely bought in JavaScript — letting the source
fill a buffer it did not allocate — was a workaround for not having cheap
ownership transfer. Rust does not need the workaround.

## The fill path was never the efficient one here

It is tempting to assume the fill-a-buffer path is the fast one — it is, after
all, the spec's BYOB-direct path. But trace a single chunk through this library's
*old* fill implementation:

```
old fill path (FD reader):
  task allocates scratch S (8192)
  source: read(fd -> S), return n
  task: queue.extend(&S[..n])             COPY 1  (S -> queue)
  later BYOB read: copy(queue -> caller B)  COPY 2  (queue -> B)
  = 2 copies + 1 allocation per pull
```

The fill path never performed the spec's trick of writing straight into the
consumer's buffer, because the queue sits between source and consumer. It was a
two-copy path wearing the costume of the spec's efficient one.

The enqueue-driven path:

```
new enqueue path (FD reader):
  source: let mut buf = ...; read(fd -> buf); enqueue(buf)   MOVE into queue (0 copies)
  later BYOB read: copy(queue -> caller B)                   1 copy (caller owns B)
  = 1 copy, 0 scratch allocation
  (a default reader returning Bytes: 0 copies)
```

Folding the two shapes into one lost nothing — the spec's optimization was never
present — and removed a copy and a per-pull allocation.

## Why would a source fill a buffer it did not allocate?

This is the crux, and it sounds bizarre until the scenario is concrete.

BYOB means "bring your own buffer." It is a *consumer* that says: "I already have
a buffer `B` — read the bytes into `B` directly." Think of a video decoder with a
pre-allocated frame buffer, or code filling a fixed-size struct. The entire point
of BYOB is that the bytes land in the consumer's buffer with no detours.

Where do the bytes come from? The source — a file, a socket. For the bytes to get
from the OS into the consumer's `B` with zero copies, something has to write
straight into `B`, and the only party holding the bytes is the source. So the
source must write into `B` — a buffer the *consumer* owns, not the source.

That is the whole motivation. "Let the source fill a buffer it did not allocate"
means "let the source write directly into the consumer's buffer, so the
consumer's buffer is the only buffer the bytes ever touch."

Side by side:

```
source allocates its own buffer (method 1 only):
  OS read --> S  (source's buffer)     write
  S --> queue                          COPY 1
  queue --> B  (consumer's buffer)     COPY 2

source fills the consumer's buffer B (method 2):
  OS read --> B  (consumer's buffer)   write, directly
```

Method 2 is the only way JavaScript can achieve true source-to-consumer
zero-copy. Because `B` belongs to the consumer, the controller has to lend it to
the source: it takes the consumer's BYOB buffer, presents it to the source as
"fill this," the source writes into it and calls `respond(n)`, and `B` returns to
the consumer filled. (`autoAllocateChunkSize` is the same machinery for default
readers — the controller pre-allocates the buffer so the source's code path is
always "fill a provided buffer.")

The "did not allocate" part is the point: the buffer has to belong to the
consumer so the bytes land in the consumer's memory in one shot.

## How this library does BYOB-direct

It is implemented — as an *opportunistic layer* on top of the queue-mediated
base, which is exactly how the WHATWG controller is structured (see below). Two
things make it a layer rather than a replacement.

First, the base has to be the queue, because the source is decoupled from the
reader. The source pushes into one central byte queue and has no idea who is
reading, how many readers there are, or what buffer they hold. That ignorance is
what keeps everything else simple:

- **tee** — two readers. The same bytes cannot be written directly into two
  buffers at once; a queue fans out by refcount trivially.
- **default reader** — no buffer is supplied at all.
- **no reader waiting yet** — the source wants to produce, but no consumer buffer
  exists. The queue lets it run ahead.
- **backpressure** — the queue decouples source pace from consumer pace.

So BYOB-direct can only be *opportunistic*: it applies when exactly one BYOB
reader is waiting and the queue is empty, and falls back to the queue otherwise.
(The single reader is guaranteed by the stream lock; the empty queue is FIFO.)

Second, the handoff has to transfer ownership. Rust cannot lend the consumer's
`&mut [u8]` to the source — the source runs in a separate task, and the borrow
checker forbids aliasing a `&mut` across that boundary (it is enforcing the exact
safety property JS enforces by detaching the `ArrayBuffer`). So the consumer moves
an owned `BytesMut` into the stream, the source fills it, and ownership moves back
— the faithful Rust analog of `ArrayBuffer` transfer. That is the `read_owned`
method: the source fills the transferred buffer via `controller.byob_request()`
and `respond(n)` for a zero-copy delivery, or `enqueue`s and the buffer is filled
from the queue (one copy) as a fallback. Queued bytes are always served first.

So the two "copy" decisions resolve differently:

- **The two-shapes fold (producer side)** — a pure win, no tradeoff. A path that
  was secretly double-copying, and never delivered the spec's benefit, was
  removed.
- **BYOB-direct (consumer side)** — a real optimization with a real cost
  (coupling), paid only on the narrow fast path and kept off the queue base, which
  still backs every other case.

## Does JavaScript suffer those same breakages?

No — and the reason matters. JavaScript is not doing *pure* direct-fill. The
`ReadableByteStreamController` runs a **hybrid**: a queue *and* an opportunistic
BYOB-direct fast path, falling back to the queue in exactly the cases that would
break pure direct-fill. It carries both:

- `[[queue]]` + `[[queueTotalSize]]` — the queue-mediated model.
- `[[pendingPullIntos]]` + `byobRequest` — the BYOB-direct machinery.

and picks per situation:

| Case | What the spec does |
| --- | --- |
| No reader waiting | `byobRequest` is `null`; the source enqueues, bytes go in the queue. Fallback. |
| Default reader | Dequeue from the queue (or, with `autoAllocateChunkSize`, the controller allocates the buffer). Queue is the backstop. |
| tee (two readers) | `ReadableByteStreamTee` reads one chunk and `CloneAsUint8Array`s it for the second branch — the same copy any implementation needs — then feeds each branch's own queue. The two-readers copy is not dodged. |
| Backpressure | The queue + `desiredSize` — the same mechanism this library uses. |

BYOB-direct (true source-to-consumer zero-copy) fires in one narrow situation:
there is a pending BYOB pull-into at the front, and the source chooses to write
into `byobRequest`'s view and call `respond(n)`. Every other time it is queue,
copy, or controller-allocated buffer.

So the opt-in fast path — used exactly when one BYOB reader is waiting and the
queue is empty, else fall back to the queue — *is* the WHATWG architecture. This
library implements both halves: the queue and the opportunistic fast-path layer
on top.

## Does this library's fast path match JavaScript's?

Yes — and more strongly than coincidence. Split the behavior in two.

The **fallback (queue) behavior matches** the spec. In every non-fast-path case
(no reader, default reader, tee, backpressure), both are queue-mediated and both
pay one copy on a BYOB read, copying from the queue into the consumer's buffer.
That is what `read(&mut [u8])` / `poll_read_into` does from `VecDeque<Bytes>` into
the caller's slice.

The **fast path** (`read_owned`) fires in the same narrow case and carries the
same limitations — because the limitations are intrinsic to the problem, not to
the implementation:

- It serves only one BYOB consumer. The same bytes cannot be written directly into
  two buffers at once — which is why tee copies, in any implementation. (Here the
  single reader is guaranteed by the stream lock.)
- It requires the queue to be empty. FIFO ordering means already-queued bytes
  must be delivered before freshly-pulled ones; if anything is queued, even a
  BYOB reader drains it first (copy), and only then can a fresh pull go direct.

Neither the spec nor this implementation can escape those, so the fast path
triggers in the same scenario and falls back in the same scenarios by necessity.
Everywhere except that one fast-path scenario, the two behave identically; the
fast path is purely additive — the one situation where a copy is saved.

## The fast path that exists, and the one that doesn't

BYOB-direct removes a real copy, so it was worth building. The analogous trick
for *default* reads — the spec's `autoAllocateChunkSize`, where the controller
allocates a buffer and has the source fill it even for a default read — removes
no copy, and is deliberately not implemented. The difference is what the consumer
asks for:

- A **BYOB reader** wants the bytes in *its own* buffer `B` — a fixed
  destination. Routed through the queue, getting them into `B` is a copy
  (queue → `B`); `read_owned` removes it by having the source write into `B`
  directly. A real copy existed.
- A **default reader** wants *a chunk* (`Bytes`) — no fixed destination. Whatever
  buffer the bytes already occupy simply *becomes* the chunk, by move. No copy
  ever existed.

`autoAllocateChunkSize` optimizes a copy that, for default reads, is not there.
Counting copies beyond the unavoidable I/O fill (the bytes must land somewhere
when read from the FD/socket):

```
source owns the buffer (what this library does):
  io.read --> buf  (source's BytesMut)        I/O fill
  enqueue(buf.freeze())                        move into queue   (0 copies)
  default read: queue.pop_front() --> Bytes    move to consumer  (0 copies)

controller owns the buffer (autoAllocateChunkSize):
  default read, queue empty: controller allocs B    allocation
  source fills B via byob_request, respond(n)        I/O fill
  B --> consumer                                     move to consumer (0 copies)
```

Both are zero copies beyond the I/O fill; the only difference is who allocated the
buffer — a wash in Rust. JavaScript keeps `autoAllocateChunkSize` for a reason
that does not translate: it lets a source be written once (always "fill the
provided buffer, respond") and serve both reader types, with the runtime handling
transfer/detach. In Rust the source contract is already uniform (`pull` enqueues
or closes; the reader pulls) and a source already owns and moves its buffer, so
the mechanism would add routing and a controller-allocated buffer to remove
nothing.

## The through-line

Three JavaScript mechanisms in this area exist for one root reason — JavaScript
has no cheap, safe ownership transfer — and Rust's `move` dissolves all three:

| JS mechanism | Why JS needs it | Why Rust does not |
| --- | --- | --- |
| Two source shapes (`enqueue` vs `respond`) | no cheap ownership transfer | `move` is the transfer |
| Fill-a-buffer pull | the source can't hand its buffer over cheaply | the source owns it and `enqueue`s |
| `autoAllocateChunkSize` (default reads) | uniform source path + detach; no copy to remove | already uniform; default reads are already zero-copy |

BYOB-direct (`read_owned`) is the one optimization here that is *not* in that
bucket: it removes a genuine copy — the queue into the consumer's fixed buffer —
which is why it was worth building and these are not.
