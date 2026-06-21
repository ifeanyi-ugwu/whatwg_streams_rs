# WPT coverage and skip rationale

The integration suite under `tests/` mirrors the WHATWG Streams Web Platform
Tests. A WPT file rarely maps one-to-one onto Rust tests: some WPT assertions
probe behaviour the spec mandates, others probe JavaScript's object model or the
browser platform. The first kind is portable and is reproduced here. The second
kind has nothing to bind to in a Rust API and is **skipped on purpose** — not as a
coverage gap.

The guiding distinction:

> A port reproduces a spec's **behaviour**, not its host language's **idioms**.
> When a WPT test asserts on an idiom rather than a behaviour, it has no Rust
> counterpart, and skipping it is correct.

Each test module carries a `// Skipped from <file> — untranslatable …` block listing
the tests it leaves out and why. This document collects the recurring reasons so the
per-file notes can stay short.

## Skip taxonomy

### 1. Promise object identity

JS promises are first-class values whose identity can be asserted with `===`.
WPT uses this to pin down memoization contracts:

```js
assert_equals(writer.ready, readyPromise);      // same stored object across reads
assert_equals(writer.abort(), writer.abort());  // repeated calls return one promise
```

In Rust, `ready()` / `abort()` return a fresh `Future` per call. The *behaviour*
these tests guard (the wait resolves/rejects correctly; a second abort does not
re-run the sink) is reproduced; the *object identity* is not a property the API
exposes, so identity assertions are dropped.

### 2. Zombie writer (released-writer observation)

`WritableStreamDefaultWriter::release_lock(self)` takes `self` **by value** — it
consumes the writer. After release, the writer is moved; touching it is a compile
error, not a runtime `TypeError`. Every WPT test that observes a writer *after*
`releaseLock()` (abort/write/close/desiredSize on a released writer, redundant
`releaseLock()`, ready-before-closed ordering on release) probes a state that is
unrepresentable in Rust. The move-by-value signature makes the stale-writer state
unreachable — a stronger guarantee than JS's runtime rejection, not a missing one.

The lock itself is real and tested: the stream is a shared handle (`Clone` over
`SharedPtr`), so ownership alone cannot enforce single-writer exclusivity — the
runtime `locked` flag does, checked in `get_writer()`. What is gone is only the
*post-release observation*, not the lock.

### 3. AbortSignal / DOM types

`WritableStreamDefaultController.signal` exposes a DOM `AbortSignal` (`.aborted`,
`.reason`, `instanceof AbortSignal`). These are Web Platform types with no Rust
equivalent; cancellation is modelled with a future being dropped / a cancellation
token instead. Tests asserting against the `AbortSignal` surface have no binding.

### 4. Thenable duck-typing

JS awaits any object with a `.then` method. WPT returns hand-rolled thenables from
`abort()` / `write()` hooks to check the Promise resolution procedure. A Rust hook
returns a concrete `Future` constrained by its trait bound; there is no
"looks-like-a-promise" ambiguity to exercise.

### 5. `this`-binding on sink methods

WPT asserts sink callbacks are invoked as methods (correct `this`) and never via
`.apply()` / `.call()`. Sinks here are Rust trait impls with an explicit `&mut self`
receiver; there is no receiver rebinding to test.

### 6. Constructors and subclassing

`new WritableStreamDefaultWriter(stream)`, failing-constructor semantics, and
`class extends WritableStream` rely on JS constructors and prototype chains.
Writers are obtained only via `get_writer()`; extension is via composition and
generics. No public constructor or prototype chain exists to misuse.

## Per-file ledger

### `writable-streams/aborting.any.js`

Actionable behaviour is covered in `writable/abort.rs`. The remaining ~47 tests are
skipped under categories above: ready-promise state/identity (§1), the `releaseLock`
family (§2), the `AbortSignal` DOM API in tests 56–65 (§3), thenable returns from
`abort()` (§4), and `this`-binding checks (§5).

### `writable-streams/general.any.js`

Covered in `writable/general.rs`, including desiredSize on errored streams,
`get_writer()` on aborted/errored streams, and the positive `locked` getter.
Skipped: released-writer observations (§2), sink-as-method / `.apply`-`.call` (§5),
and subclassing (§6).

### `writable-streams/write.any.js`

Covered in `writable/write.rs`, including close-waits-for-pending-writes, large-queue
draining, and write-rejection clearing the queue and pending close. Skipped:
write-to-released-writer (§2), thenable from `write()` (§4), and manual /
failing-constructor writer tests (§6).

### `piping/general.any.js`

Covered in `piping/mod.rs`: `pipe_to` locks the destination for the pipe's duration
and unlocks on completion, and fails when the destination is already locked.

A note on locking, because the Rust shape differs from JS: `pipe_to(self, &dest)`
consumes the readable by value and borrows the writable. The readable is therefore
locked by *move* — "pipeTo must fail if the RS is locked" is compile-enforced, not a
runtime check — while the writable keeps a runtime lock that the tests observe.

Skipped: brand checks on the `this`/argument types (Rust generics fix types at
compile time); the option-getter side-effect test (`StreamPipeOptions` is a plain
boolean struct, no accessors); and null-options (the Rust `None`, exercised by nearly
every pipe test).

### `piping/flow-control.any.js`

Covered in `piping/mod.rs`: backpressure block-then-drain and in-order delivery, plus
piping into a zero-HWM destination — which must read nothing (no read-ahead) and
reject once the destination errors. The zero-HWM case surfaced a construction bug:
backpressure was hardcoded off at init, so a fresh HWM-0 writable wrongly reported
`ready`. The remaining tests overlap the covered backpressure behaviour.

### `piping/close-propagation-forward.any.js` + `error-propagation-forward.any.js`

Forward close/error propagation, the prevent-option variants, and rejected
close/abort propagation are covered by the existing piping suite. The added tests
cover the one behaviour those did not: shutdown must wait for an in-flight write to
drain before closing (close-forward) or aborting (error-forward) the destination —
the pipe's `ready()` gate enforces this.

These files are large because they cross every prevent-option permutation with
fulfilled/rejected promises and three source timings. The behaviourally distinct
cases reduce to a handful: "starts closed" collapses to the empty-source close test,
"starts errored" to the source-error abort test, and "writable errors while flushing"
to the rejected-close test. Permutations that only re-run a covered behaviour with a
different option flag are not duplicated.

### `piping/abort.any.js`

Signal-driven abort is covered across the existing piping suite: a fired signal
aborts the pipe, a pre-fired signal rejects immediately, the prevent-options gate the
cancel/abort, and abort is a no-op after the readable has already terminated. Added:
when the signal fires and `sink.abort()` rejects, `pipeTo` returns that rejection
(preferred over a `source.cancel()` rejection) rather than a generic Aborted error —
the signal-abort path had been discarding both action results.

Skipped: the teed-byte-stream priority case (byte sources are a separate area), and
the JS getter/duck-typing checks shared with pipe-through.

### `piping/close-propagation-backward.any.js` + `error-propagation-backward.any.js`

Backward error propagation — a destination that errors (during or after piping)
cancels the source, with the reason passed through and rejected-cancel propagated —
is covered by the existing piping suite. Backward *close* propagation is a documented
divergence: `pipe_to` into an already-closed destination completes `Ok` without
cancelling the source, where the spec cancels it. The behaviour is pinned by
`pipe_to_already_closed_dest_returns_ok_without_cancel` and tracked for later debate;
distinguishing a pipe-initiated close from an external one in the loop's close-arm is
the blocker.

### `piping/multiple-propagation.any.js`

Combinations of an errored/closed source with an erroring/errored/closed/closing
destination. Each combination resolves to the single-direction propagation behaviours
already covered (forward abort/close, backward cancel) composed together; the matrix
adds no behaviour not exercised by those.

### `transform-streams/general.any.js` + `errors.any.js`

The transform suite already covers passthrough/type-change/filter, terminate, cancel,
backpressure, and desiredSize. Added: a flush() error errors both sides; a start()
rejection errors the stream; the writable abort() reason reaches the readable error;
close() waits for an in-flight transform before closing the readable; and enqueue()
after terminate() is rejected. All passed — the transform error and ordering paths
hold.

Skipped: identity-default construction (the trait requires a transform method);
method-as-method / `.apply`-`.call` `this`-binding; the `readableType`/`writableType`
constructor guards; synchronous constructor throwing (construction is infallible — a
start() rejection errors the stream instead); subclassing; and properties /
patched-global introspection — all JS object-model concerns. The reentrant-strategies
and lipfuzz files exercise JS re-entrancy and a specific string transform that add no
distinct Rust behaviour.

### `transform-streams/backpressure.any.js`, `cancel.any.js`, `flush.any.js`, `terminate.any.js`, `strategies.any.js`, `reentrant-strategies.any.js`

Backpressure, cancel/abort propagation, terminate, and flush behaviour are covered by
the existing transform suite. (Note: backpressure's "writer.closed should resolve after
readable is canceled" tests assert a *rejection* in their bodies — the title is a WPT
naming quirk — matching the covered reject-on-cancel behaviour.)

`strategies.any.js`: the default strategies are spec-correct — writable HWM 1, readable
HWM 0 — verified by `default_strategy_hwms`. (The readable default was previously HWM 1;
correcting it to 0 means a transform with no explicit readable strategy backpressures
immediately, so tests that await a write before reading set an explicit readable HWM 1.)

Skipped as untranslatable: all of `reentrant-strategies.any.js` (every test calls a
stream method *inside* `size()`, but `QueuingStrategy::size(&self, &T) -> usize` is a
pure function with no stream access); and the `strategies.any.js` size-function-throws /
RangeError-for-bad-HWM cases (size is an infallible trait method; HWM is `usize`).

`cancel.any.js` tests 6/7 — where the transformer's `cancel()` calls
`controller.error()` — are not expressible as written, but the distinction is worth
stating precisely. JS `cancel(reason)` is not passed the controller either; those
tests reach it by capturing the controller from `start()` in a closure. The Rust
`start()` receives `&mut TransformStreamDefaultController`, a borrow that cannot be
stored, and the controller is not `Clone`, so it cannot be captured into `cancel()`.
The *observable outcome* those tests assert — a `cancel()`/`abort()` that signals
failure makes the cancel/abort/close reject — is reached idiomatically by returning
`Err` from `cancel()`, covered by `cancel_that_throws_rejects_readable_cancel` and
`abort_that_throws_rejects_writable_abort`. (Making the controller `Clone` — its
fields are all `SharedPtr` — would make the error()-from-cancel mechanism expressible
too, if that exact path is ever wanted.)

## Byte streams (`readable-byte-streams/`)

The Rust byte API is deliberately abstracted: a source implements
`pull(&mut self, controller, buffer: &mut [u8]) -> usize` — fill the provided buffer,
return bytes written — plus `controller.enqueue(Vec<u8>)` / `close()` / `error()` /
`desired_size()`. The reader side is `reader.read() -> Option<Vec<u8>>` (default) and
`byob.read(&mut [u8]) -> usize` (BYOB).

This collapses the spec's BYOBRequest machinery. There is no `byobRequest`,
`respond()`, `respondWithNewView()`, or `autoAllocateChunkSize`, and no
`ArrayBuffer`/`TypedArray` object model. So the large majority of
`general.any.js` (85), and all of `bad-buffers-and-views`,
`enqueue-with-detached-buffer`, `non-transferable-buffers`, and
`construct-byob-request`, have no Rust API surface — they test buffer detaching,
transferable buffers, view element types (`Uint16Array`/`Uint32Array`),
`respond`/`respondWithNewView` ordering, and direct BYOBReader construction.

Covered behaviours (existing suite plus additions): default and BYOB reads, EOF,
cancel with reason, start/pull rejection erroring reads, reader-lock exclusivity, a
BYOB read with a view smaller than the queued data serving the remainder on the next
read, and the controller's post-close guards. The last surfaced a fix: `close()` now
rejects on an already-closed stream (it had returned Ok), matching `enqueue()`.

`read-min.any.js` (`read(view, {min})`) is skipped as untranslatable. The `{min}`
option resolves a BYOB read only once at least N bytes are filled; both endpoints it
spans are already reachable — `byob.read(&mut buf)` resolves on the first partial fill
(spec `min: 1`), and `read_exact` over the `AsyncRead` impl fills the whole buffer
(spec `min: view.length`). Arbitrary `1 < min < len` is a read loop. Rust's async-I/O
surface (tokio/futures `AsyncRead`) offers exactly `read` (partial) and `read_exact`
(fill) — "read at least N" is not an idiom there, so a `read_min` knob would import a
JS-ism with no precedent. The option is part of the same `read(view, options)` surface
already skipped (byobRequest/respond/autoAllocate), and its validation assertions
(`min: 0` throws, `min > length` throws) only bind to a parameter that would exist
solely to satisfy them.

`tee.any.js` for byte streams: `tee()` on a byte stream yields two **byte** branches
(`ReadableStream<Vec<u8>, _, ByteStream>`), so a consumer can take a BYOB reader on a
branch and read into their own buffer — pinned by `byte_tee_branch_supports_byob_reader`.
Both branches drain the full content independently (each owns its `Vec<u8>`), cancel-one
keeps the other live, and canceling both fires the source's `cancel()` exactly once —
covered by the byte-tee tests plus the generic tee suite in `readable/tee.rs` (the
coordinator and channels are shared between default and byte tee).

This was previously a divergence (byte tee yielded *default* branches). It is now
implemented: `tee()` stays universal, and the `TeeBuilder` terminal methods are split by
pinning `S` to a concrete type (`DefaultStream` vs `ByteStream`), so no specialization is
needed. The handing of an owned `Vec<u8>` to each branch is exactly the spec's
`CloneAsUint8Array` — the spec copies for branch 2 precisely because BYOB reads detach the
buffer, so branches cannot share one. The only genuinely untranslatable part is the
JS-level buffer-aliasing assertions, which do not exist when each branch owns its bytes.

### `piping/pipe-through.any.js`

`pipe_through` data flow, close propagation, and chaining are covered. The
prevent-option cases reduce to the `pipe_to` prevent tests (`pipe_through` pipes the
source into the transform's writable). Skipped: duck-typed pass-through (JS structural
typing of `{readable, writable}`); unhandled-rejection accounting; not-calling-`pipeTo`
on the prototype; getter rethrow / option-getter side effects; and the locked-`this`
check (the readable is consumed by move).
