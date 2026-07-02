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
`.reason`, `instanceof AbortSignal`, event-target semantics). The behavioural core
is modelled in Rust: `controller.abort_future()` / `with_abort()` cover `.aborted`
(react to an in-flight abort) and `controller.abort_reason()` covers `.reason` (the
reason carried end to end). What stays untranslatable is the DOM object *surface* —
`instanceof AbortSignal`, `addEventListener`, `throwIfAborted()` — which is a host
type, not a stream behaviour. Tests asserting against that surface have no binding.

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

### `readable-streams/general.any.js` + `default-reader.any.js` + `cancel.any.js`

Covered in `readable/`: single and repeated reads, close/error/cancel propagation, the
controller `desired_size`/`close`/`enqueue`/`error` guards, and reader-lock exclusivity.
Added behaviours: two concurrently-pending `read()`s resolve together with `None` on close
and reject with the same error on error (`two_pending_reads_*`); cancelling before `start()`
finishes prevents `pull()` from ever running (`cancel_before_start_finishes_prevents_pull`);
and `desired_size` reflects HWM minus the committed queue depth, not just the empty-queue
case (`controller_desired_size_reflects_queue_depth`).

`desired_size`'s synchronous per-enqueue decrement and negative overshoot (WPT
`count-queuing-strategy-integration.any.js`) are **not portable**. `controller.enqueue()` is
an async channel message the stream task commits later, not a synchronous queue mutation, and
`pull()` fires only while `desired_size > 0` — so a negative `desired_size` is never surfaced
through the source API. Only the committed, quiescent depth is observable, which the added
test pins. `floating-point-total-queue-size.any.js` is untranslatable for the same integer
reason: the queue total is a `usize` and `QueuingStrategy::size(&T) -> usize`, so the f64
precision drift and clamp-to-zero it exercises cannot arise. `constructor.any.js` is JS
constructor-ordering (§6); `read-task-handling` / `templated` are microtask-scheduling and
harness meta with no binding.

### `writable-streams/aborting.any.js`

Actionable behaviour is covered in `writable/abort.rs`, including: aborting before `start()`
finishes rejects both the pending `ready()` and the pending `write()` and never calls the
sink's `write()` (`abort_before_start_rejects_pending_ready_and_write` — the companion
`writer.ready === readyPromise` identity assertion is §1). The remaining ~47 tests are
skipped under categories above: ready-promise state/identity (§1), the `releaseLock`
family (§2), the `AbortSignal` DOM API in tests 56–65 (§3), thenable returns from
`abort()` (§4), and `this`-binding checks (§5).

The "abort() rejects with the rejection returned from close()" test is behaviour (abort
adopting a queued close's rejection), not a §1 promise-identity idiom; it is not yet ported.

### `writable-streams/close.any.js`

Covered in `writable/close.rs`: close drains queued writes first, close on an
already-closed/errored stream rejects, a second in-flight close rejects, `sink.close()`
rejection errors the stream, and `abort()` during an in-flight close resolves while the
stream errors (`abort_during_pending_close_resolves`).

The two "sink calls `controller.error()` while close is in-flight" tests (the error is
ignored, sync and async) are **untranslatable** for the same reason as transform
`cancel.any.js` tests 6/7: `WritableSink::close(self)` receives no controller, and
`WritableStreamDefaultController` is not `Clone`, so a sink cannot hold a controller to call
`error()` from inside `close()`. A sink `close()` that returns `Err` (the throwing-close
path) is the expressible analogue and is covered.

### `writable-streams/{count,byte-length,floating-point}-queuing-strategy.any.js`, `reentrant-strategy.any.js`

Count/byte-length strategy construction is exercised by every `CountQueuingStrategy::new`
call. `floating-point-total-queue-size.any.js` is untranslatable (integer `usize` queue
total, no f64 drift), and `reentrant-strategy.any.js` falls under the reentrancy-inside-`size()`
skip (see transform strategies below) — `QueuingStrategy::size(&self, &T) -> usize` is a pure
function with no stream access.

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

Covered in `piping/mod.rs`: backpressure block-then-drain and in-order delivery, piping
into a zero-HWM destination — which must read nothing (no read-ahead) and reject once the
destination errors — and the read-ahead complement: a HWM > 1 destination is filled ahead,
so the pipe reads further chunks into the writable queue while the first write is still in
flight (`pipe_to_reads_ahead_into_hwm_gt_1_destination`). The zero-HWM case surfaced a
construction bug: backpressure was hardcoded off at init, so a fresh HWM-0 writable wrongly
reported `ready`. The remaining tests overlap the covered backpressure behaviour.

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
the signal-abort path had been discarding both action results. Also added: the
signal's reason propagates into `source.cancel()`, `sink.abort()`, and the `pipeTo`
rejection — `StreamPipeOptions::signal` is a reason-carrying `AbortSignal`, so the
pipe forwards the real reason instead of a hardcoded placeholder.

Skipped: the teed-byte-stream priority case (byte sources are a separate area), and
the JS getter/duck-typing checks shared with pipe-through.

### `piping/close-propagation-backward.any.js` + `error-propagation-backward.any.js`

Backward error propagation — a destination that errors (during or after piping) cancels
the source, with the reason passed through — is covered by the existing piping suite. When
that `source.cancel()` itself rejects, its rejection (not the destination's write error) is
what `pipe_to` rejects with, matching the spec's shutdown-with-action finalize step and the
already-fixed signal-abort path; this closed a divergence where the non-signal backward path
discarded the cancel rejection, and is pinned by
`pipe_to_cancel_rejection_on_dest_error_propagates_backward`. Backward *close* propagation is
covered too: `pipe_to` into an already-closed (or externally-closed) destination cancels the
source and rejects — with a throwing `source.cancel()` surfacing its rejection, pinned by
`pipe_to_cancel_rejection_on_dest_close_propagates_backward` — see also
`pipe_to_closed_dest_cancels_source_and_rejects`. The pipe's own close (source exhausted →
`writer.close()`) is a separate arm that returns directly, so this only affects a destination
closed out from under the pipe.

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
naming quirk — matching the covered reject-on-cancel behaviour.) Added: a write parked on
readable backpressure rejects when the writable is aborted (`errors.any.js` — the abort-side
complement to the covered cancel-side `readable_cancel_clears_backpressure_and_closes_writer`),
pinned by `abort_rejects_write_parked_on_backpressure`; and when `writable.close()` wins a
race with a parallel `readable.cancel()`, `flush()` runs, `transformer.cancel()` is not
called, and both sides resolve cleanly (`flush.any.js` — `close_flush_wins_over_parallel_cancel`),
the close-wins counterpart to the cancel-wins `close_rejects_when_parallel_cancel_throws`.

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
`enqueue-with-detached-buffer`, `non-transferable-buffers`, `respond-after-enqueue`, and
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

`piping/transform-streams.any.js` (identity-transform close propagation) reduces to the
covered `pipe_through_close_propagates`. `piping/general-addition.any.js` (enqueue must not
synchronously drive the write algorithm) is implicit in the async pipe model. `piping/
throwing-options.any.js` and `piping/then-interception.any.js` are whole-file skips —
throwing option getters / getter-evaluation order (`StreamPipeOptions` is a plain bool
struct) and `Object.prototype.then` thenable scheduling, respectively.

## Exhaustive file-level accounting

A test-by-test pass over the entire live WPT streams suite confirmed every file is
accounted for. The per-area sections above cover the primary files; the remainder are
listed here with their disposition so nothing is silently missing.

### Known behavioural divergences

Two portable spec behaviours are currently *not* matched. Each has an `#[ignore]`d test
asserting the spec-correct outcome, kept as executable documentation:

- **transform `errors.any.js`: an exception from `transform()` after `terminate()` must
  error the readable with the thrown error.** The readable closes eagerly on `terminate()`
  even with a chunk still queued, so the following `error()` is a no-op and the readable
  closes cleanly instead of erroring. The spec keeps the stream `readable` (closing) until
  the queue drains, so a later `error()` still applies. The fix is to defer the readable's
  `Closed` transition until the queue drains — a core change with wide blast radius
  (`closed()` timing when data is queued at close). Test:
  `transform_throw_after_terminate_errors_readable`.
- **writable `error.any.js`: a surplus `controller.error()` must be a no-op (first error
  wins).** The `ControllerMsg::Error` handler overwrites the stored error unconditionally
  (last-wins). A naive first-wins guard regresses the `controller.error()`-then-write-
  rejection case (currently spec-correct: the write promise carries its own error while the
  stored error is the controller's), because both paths share the handler and depend on
  async message ordering. A correct fix needs cross-path first-wins error precedence. Test:
  `surplus_controller_error_is_noop_first_wins`.

Related accepted decision: writable `abort()` during an in-flight *rejecting* `close()`
(WPT "abort() should be rejected with the rejection returned from close()") falls under the
already-accepted abort-during-close divergence (see
`abort() during close skips sink.abort()` — abort resolves and `sink.abort()` is not
called). Not re-litigated here.

### Writable files

- `start.any.js` — covered in `writable/start.rs` (write/close deferred until start; start
  rejection errors the stream; `controller.error()` during start).
- `constructor.any.js` — controller-to-`start`/`write` and HWM→desiredSize covered;
  controller-not-to-`close` is structural (`close(self)` takes no controller); the rest are
  §6 constructor/brand and Infinity-HWM (HWM is `usize`).
- `error.any.js` — `controller.error()` errors the stream (covered); surplus-error first-wins
  is the divergence above; `error()` on a closed/errored stream is untranslatable (no
  controller survives the terminal transition; controller is not `Clone`).
- `bad-underlying-sinks.any.js` — throwing `close`/`write`/`abort` → reject (covered in
  close/write/abort.rs); constructor getter-throws and non-function props are §6.
- `bad-strategies.any.js` — whole-file skip: `size` is infallible `-> usize`, HWM is `usize`;
  throwing getters are §6.
- `properties.any.js` — whole-file skip: sink-method arity is fixed by the trait signature
  (this file is the direct evidence `close()` takes zero args); §6 prototype.
- `garbage-collection.any.js` — whole-file skip: no GC; "stays alive while a write is pending"
  is a structural property of the ownership model (`SharedPtr` holds the task).

### Transform files

- `lipfuzz.any.js` — whole-file skip: a concrete JS string-substitution transform over 20
  vectors; its passthrough + flush-emits-remainder core is already covered.
- `properties.any.js` — whole-file skip: arg-count / prototype-chain introspection.
- `patched-global.any.js` — whole-file skip: constructor must avoid patched globals and
  `Object.prototype` setters.
- `invalid-realm.tentative.window.js` — whole-file skip: detached-realm lifetime; no analogue.

### Byte-stream files (plus count corrections)

- `patched-global.any.js` — whole-file skip: a patched `then()` observing `byobRequest`.
- `owning-type.tentative.any.js` (and the `-message-port` / `-video-frame` siblings) — the
  successor to the now-removed `owning-byte-source.any.js`; whole-file skip: the tentative
  owning type + `ArrayBuffer` transfer (Rust already moves owned bytes by value).
- `templated.any.js` — behaviourally covered (empty/closed/errored reads via the byte suite);
  harness meta otherwise.
- Count corrections to the byte section above: `general.any.js` is **101** tests (not "85");
  `construct-byob-request.any.js` is a **16**-case matrix (not one test). Both remain
  whole-file / near-whole-file skips (buffer model).

### Readable files

- `bad-strategies.any.js` — whole-file skip: infallible `size -> usize`, `usize` HWM (same
  bucket as the writable equivalent).
- `bad-underlying-sources.any.js` — throwing `pull`/`cancel` and enqueue/close-after-terminal
  are covered; throwing getters are §6.
- `from.any.js`, `async-iterator.any.js` — whole-file skip: `ReadableStream.from` and the
  async-iterator protocol have no Rust surface.
- `patched-global.any.js`, `garbage-collection.any.js`, `read-task-handling.window.js` —
  whole-file skip: patched globals / GC timing / browser microtask-scope.
- top-level `queuing-strategies.any.js` — ported by `queuing_strategies/mod.rs` (Count
  `size == 1`, ByteLength `size == chunk len`, valid-HWM construction); the JS
  function-object introspection is §6.
- `crashtests/*`, `cross-realm-*`, `idlharness.any.js`, `owning-type*.tentative.*` — out of
  scope (crash regressions / cross-realm / IDL harness / tentative features).

### Identified portable gaps not yet ported

Distinct spec behaviours the audit surfaced that are neither covered nor a documented skip,
kept here so the accounting is complete (candidates for a future pass):

- piping `close-propagation-forward`: erroring the writable while flushing queued writes must
  error `pipeTo` (an in-flight `write()` rejects *during* the close sequence).
- piping `error-propagation-backward`: when the final write rejects after the source has
  already closed, the pipe rejects but must not cancel the already-closed readable.
- piping `abort`: a late signal is a no-op after the *readable* errored with pending writes,
  and after the *writable* errored (only the readable-errored, no-pending-writes branch is
  covered).
- readable `tee`: the coordinator's pull-scheduling bound ("pull only to fill the emptiest
  branch queue"; stop pulling once the source errors) — tied to the `BackpressureMode`
  extension's semantics.
- byte `general`: a parked byte `read()` then `error()`; `desired_size == 0` when closed
  (both low-value — shared code paths already covered generically).
- writable `aborting` / `write`: after `abort(reason)`, a later in-flight `write()` that
  rejects must not overwrite the stored error — `closed()` rejects with the abort reason
  while the `write()` promise carries its own error (abort/write error precedence).
- transform `general`: closing a HWM-0 writable with no reader closes both sides cleanly
  "even with backpressure" (likely already passing via the default HWM-0 close path; a
  one-line variant on `closing_writable_closes_readable` would pin it).
- readable `general`: `pull()` is called exactly once on an idle started stream (no reads,
  no enqueue) — the pull-once-after-start lower/upper bound.
- readable `bad-underlying-sources`: a chunk already committed to the queue is still
  delivered even if the *next* `pull()` throws (the error surfaces only on the later read).

### Stale test-comment labels (no coverage impact)

The piping test module uses section headers `error-propagation-via-abort.any.js` /
`error-propagation-via-cancel.any.js`; the real WPT files are `error-propagation-forward` /
`-backward`. Labels only.
