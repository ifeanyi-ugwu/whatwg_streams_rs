# ADR 0001 — Zero-copy byte streams via `bytes::Bytes`

- Status: Accepted
- Date: 2026-06-27

## Context

Readable byte streams (`type: "bytes"`) currently store their queue as a flat
`VecDeque<u8>` and exchange chunks at the public boundary as `Vec<u8>`. Every
byte is copied between two and three times on its way from a source to a
consumer:

- A pulling source fills a controller-owned scratch buffer (`vec![0u8; 8192]`) — copy 1.
- `enqueue_data(&scratch)` copies that into the internal `VecDeque<u8>` — copy 2.
- A default read materializes a fresh `Vec<u8>` out of the byte pool — copy 3.

A push source (`controller.enqueue(Vec<u8>)`) skips copy 1 but still pays 2 and
3, because `enqueue` takes ownership of the `Vec` only to copy its bytes into the
pool and discard it. `tee()` fans out by cloning the full `Vec<u8>` into every
branch, so an N-way tee copies each byte N more times.

The chunk type is destroyed into individual bytes the instant it crosses the
boundary, because byte-level granularity — a reader asking for N bytes and
getting them regardless of how the producer chunked the input — requires a `u8`
buffer. That requirement is real, but the *representation* of that buffer as
owned `u8` runs, copied at every hop, is not.

## Decision

Adopt `bytes::Bytes` as the byte chunk type. The internal queue becomes
`VecDeque<Bytes>`: a queue of immutable, refcounted byte chunks rather than a
flat pool of owned bytes.

`Bytes` is hardcoded rather than exposed as a generic `T: BytesLike` bound. The
only types anyone would plug into such a bound are `Bytes`/`BytesMut`, and a type
parameter would propagate bounds across the entire readable surface (tee,
builder, reader, pipe, transform interop) for no additional capability. `Bytes`
provides exactly the primitives the byte core needs: cheap `clone` (refcount),
`split_to`/`advance` (zero-copy slicing for byte-granular reads), and
`From<Vec<u8>>` / `From<&'static [u8]>`.

Boundary shapes:

- **Enqueue** accepts `impl Into<Bytes>`. A caller already holding `Bytes`
  transfers it in with no byte copy; `Vec<u8>`, `&'static [u8]`, and `String`
  convert for ergonomics.
- **Default reads** return `Bytes` — one queue entry per read (see semantics
  below).
- **BYOB reads** keep the `poll_read_into(&mut [u8])` shape. The destination is
  caller-owned, so this path copies by definition and is unchanged.
- **Sources** gain a second shape. The existing fill-a-buffer pull
  (`pull(&mut self, controller, buf: &mut [u8])`) stays for sources that read
  into scratch (e.g. an FD); a `Bytes`-returning shape is added for sources that
  already hold buffers (e.g. an HTTP body, an mmap), so the pull side can be
  zero-copy too.

## Why this preserves WHATWG semantics

The WHATWG byte stream is already a zero-copy design; it expresses zero-copy
through a mechanism JS has and Rust does not need.

In JS, `controller.enqueue(view)` and BYOB fills **transfer** the underlying
`ArrayBuffer` — it is detached from the producer, which can no longer touch it.
Detachment is the spec's mechanism for moving bytes without copying while staying
memory-safe: it guarantees no two parties alias the same mutable buffer. Two Rust
constructs encode this:

- ArrayBuffer transfer/detach ≙ a Rust **move**: ownership leaves the producer,
  enforced by the compiler instead of a runtime "detached" flag.
- The spec's shared immutable byte sequence ≙ **`bytes::Bytes`**: immutable,
  refcounted, freely shareable.

So `Bytes` is the faithful Rust encoding of what the spec models. The prior
copy-everything `VecDeque<u8>` design was the *less* faithful part.

| Path | Behavior | Compliance |
| --- | --- | --- |
| Default read granularity | Returns one queue entry per read | **More** faithful — the spec's `PullSteps` dequeues exactly one entry; the prior pool coalesced across enqueues |
| BYOB read | Copies available bytes into caller's `&mut [u8]`, partial allowed | Unchanged — copy is intrinsic to a caller-owned destination |
| `tee()` | Refcount-shares the same `Bytes` into both branches | Observably identical — see below |
| Backpressure / desired size | `queueTotalSize = Σ chunk.len()`, `desiredSize = HWM − queueTotalSize` | Unchanged — byte-measured |
| Source provides bytes | Two shapes: fill-buffer and return-`Bytes` | Both are spec paths — see below |

### Tee: refcount instead of clone

The spec's `ReadableByteStreamTee` copies the chunk for the second branch
(`CloneAsUint8Array`), because JS `ArrayBuffer`s are mutable: if both branches
shared one buffer, a consumer mutating its view would corrupt the other branch.
The spec copies to *simulate* immutable sharing that JS cannot express. `Bytes`
is genuinely immutable shared memory, so refcount-sharing both branches yields
the identical observable result — both see the same bytes, neither can corrupt
the other — without the copy. The copy is elided because the type system
guarantees what JS must enforce by copying.

### Pull: two source shapes are both in the spec

The spec gives a source two ways to provide bytes:

- `controller.enqueue(view)` — the source produced its own buffer and transfers
  it in (zero-copy). The `Bytes`-returning source shape maps onto this.
- A BYOB request + `respond(n)` — the controller hands the source a buffer (via
  `autoAllocateChunkSize`) and the source fills it. The fill-a-buffer pull maps
  onto this; its `8192` scratch is an implicit `autoAllocateChunkSize`.

Supporting both shapes is more complete, not less compliant.

## Guardrails

These invariants keep the implementation spec-compliant:

1. Default reads hand out **immutable** chunks (`Bytes`, never `BytesMut` or a
   `&mut` into shared memory). Tee's refcount-sharing is sound only because the
   shared bytes are read-only.
2. Queue size is measured in bytes (sum of chunk lengths).
3. Partial reads and one-chunk-per-read for the default reader are permitted and
   expected.
4. Closing with bytes still queued drains the queue before signalling EOF.

## Consequences

Wins:

- Enqueue, owned-chunk reads, and tee drop from 2–3 byte copies to zero
  (move/refcount). Allocation churn (per-pull and per-read scratch `Vec`s) is
  eliminated on those paths.
- Pipe-through is zero-copy without any writable-side change: `pipe_to` is
  homogeneous in the chunk type (`WritableStream<T, Sink>` where
  `Sink: WritableSink<T>`), and the writable core was already chunk-generic, so a
  byte readable pipes its `Bytes` handles straight into any `WritableSink<Bytes>`.
- Native interop with the `bytes`-based async ecosystem (hyper, tonic, object
  stores) without a `.to_vec()` at the boundary.

Costs and limits:

- New dependency on `bytes`.
- `Bytes` uses atomic refcounting even under the single-threaded `local` feature.
  A non-atomic variant is not worth the complexity.
- BYOB reads into a caller-owned `&mut [u8]` remain copy-bound — zero-copy cannot
  help a destination the caller owns. The contiguous `as_slices` fast path is
  replaced by draining across front chunks (same total bytes copied, slightly
  more bookkeeping).
- Byte-granular reads stay zero-copy only under return-what's-at-the-front
  semantics (`Bytes::split_to`/`advance`, which are offset math). A demand for a
  single contiguous owned buffer larger than the front chunk forces coalescing —
  one copy. WHATWG permits partial returns, so this is rarely forced.

Alternatives rejected:

- **Keep `Vec<u8>`.** Simplest, but leaves every hop copying and tee cloning.
- **Generic `T: BytesLike` bound.** Cosmetic flexibility; propagates a type
  parameter and bounds across ~4400 lines for no capability the `VecDeque<u8>`
  core or the hardcoded-`Bytes` core does not already provide.
- **Relax only `enqueue(impl AsRef<[u8]>)`.** Removes the input-side ergonomic
  pinch with a one-line change, but delivers no zero-copy: the byte still lands
  in `VecDeque<u8>` by copy. Adequate only if zero-copy is not a goal.

## Implementation plan

1. **`byte_state.rs` internals.** Replace `VecDeque<u8>` with `VecDeque<Bytes>`;
   rewrite `poll_read_into` to drain across front chunks; add `enqueue_bytes`
   (zero-copy push) and `poll_read_chunk` (one-entry default read returning
   `Bytes`). Keep existing `ByteStreamStateInterface` signatures so the rest of
   the crate still compiles.
2. **Controller / reader / tee.** Thread `Bytes` through the readable surface:
   `enqueue(impl Into<Bytes>)`, default reads returning `Bytes`, tee branches
   sharing by refcount. The byte stream's chunk type becomes `Bytes`. The
   writable side needs no change — `pipe_to` is homogeneous in the chunk type and
   the writable core is already chunk-generic, so pipe-through into a
   `WritableSink<Bytes>` is zero-copy automatically. `Bytes` is re-exported at the
   crate root so downstream code can name it.
3. **Source contract.** Add the `Bytes`-returning pull shape alongside the
   existing fill-a-buffer shape (the pull side still copies until then).

## Validation

WPT byte-stream coverage (231 passing at the time of this decision) must stay
green. The default-reader granularity change (coalesced → one entry per read) is
the one observable shift; the byte default-reader tests are the place to confirm
no regression.
