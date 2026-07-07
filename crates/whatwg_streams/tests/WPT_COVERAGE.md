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

### `readable-streams/general.any.js` + `default-reader.any.js` + `cancel.any.js` + `bad-underlying-sources.any.js`

Covered in `readable/`: single and repeated reads, close/error/cancel propagation, the
controller `desired_size`/`close`/`enqueue`/`error` guards, and reader-lock exclusivity.
Added behaviours: two concurrently-pending `read()`s resolve together with `None` on close
and reject with the same error on error (`two_pending_reads_*`); cancelling before `start()`
finishes prevents `pull()` from ever running (`cancel_before_start_finishes_prevents_pull`);
and `desired_size` reflects HWM minus the committed queue depth, not just the empty-queue
case (`controller_desired_size_reflects_queue_depth`).

"should only call pull once upon starting the stream" (`pull_called_once_on_idle_started_stream`)
surfaced and fixed a real busy-loop divergence: an idle started stream (default HWM 1, whose
`pull()` enqueues nothing) must call `pull()` exactly once and then stop. The pull gate was
level-triggered on `desired_size > 0`, so it re-fired forever (measured ~25k pulls in 50 ms —
a pegged core). Fixed by making the gate edge-triggered via a task-local `needs_pull` flag,
mirroring the spec's `ReadableStreamDefaultControllerCallPullIfNeeded`/`[[pullAgain]]`: armed
after start, after each enqueue, and after each read's pull steps; consumed when serviced. The
autonomous HWM-fill loop (`pull_loops_autonomously_until_hwm_with_no_reads`) still holds — an
enqueueing pull re-arms the flag, which is exactly `[[pullAgain]]`.

`bad-underlying-sources.any.js` "read should not error if it dequeues and pull() throws"
(`read_succeeds_when_dequeue_triggers_throwing_pull`): a read served from the committed queue
resolves with that chunk even though the follow-up `pull()` it triggers throws — the error
surfaces only via `closed()`, never retroactively failing the already-dequeued read. Already
correct (the Read command sends the chunk before the pull gate fires); kept as a regression test.

`desiredSize` at the terminal transitions is covered and was fixed: closing an empty queue reports
`0` (not `null`) and erroring reports `null`, read synchronously from inside `start()` — `close()`
and `error()` set their request flags synchronously, so `desired_size()` reflects the transition in
the same frame (`default_controller_desired_size_when_closed_and_errored`). A close whose queue is
still draining stays `readable`, keeping `HWM − queue` until it empties. `desired_size()` previously
collapsed closed to `None`, the same divergence fixed for byte streams.

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

Abort/write error precedence is covered. A write already dispatched to the sink finishes with its
own result even while an abort is pending — its promise carries the write's outcome, not the abort
error — while `closed()` rejects with the abort reason, pinned first-wins so a later in-flight
write rejection cannot overwrite it (`abort_error_not_overwritten_by_later_write_rejection`).
Writes still queued behind the in-flight one reject with the abort reason
(`abort_rejects_queued_writes_but_in_flight_finishes`, and through a transform
`abort_rejects_queued_write_through_transform`).

### `writable-streams/close.any.js`

Covered in `writable/close.rs`: close drains queued writes first, close on an
already-closed/errored stream rejects, a second in-flight close rejects, `sink.close()`
rejection errors the stream, and `abort()` during an in-flight close resolves while the
stream errors (`abort_during_pending_close_resolves`).

The two "sink calls `controller.error()` while close is in-flight" tests (sync and async) are
now covered: `WritableStreamDefaultController` is `Clone`, so a sink captures it in `start()` and
calls `error()` from inside `close(self)` even though `close` receives no controller
(`controller_error_during_close_is_discarded`, `controller_error_during_inflight_close_is_discarded`).
Both surfaced a divergence, now fixed: the error errored the stream instead of being discarded. The
`ControllerMsg::Error` handler now no-ops unless the stream is still `Writable` with no close in
flight, so a successful close wins and clears the error (spec `WritableStreamFinishInFlightClose`).

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

The stored error is first-wins: a surplus `controller.error()` on an already-errored stream is a
no-op (`surplus_controller_error_is_noop_first_wins`), and a `controller.error()` raised inside a
sink `write()` that then rejects still wins over that write's own rejection as the stored error —
its message ordering is resolved by draining controller messages before recording the write
rejection (`controller_error_before_write_rejection_wins_stream_error`). Each write promise still
carries its own error; only the shared stored error (`closed()`/`ready()`) is pinned to the first.
A `controller.error()` on an otherwise idle stream still settles a waiting `closed()`
(`controller_error_while_idle_rejects_closed_promptly`).

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

The close-forward tail case is also pinned: when the source has already closed and the pipe is
flushing pending writes before closing the destination, an in-flight write that *rejects* mid-
flush errors `pipe_to` with that write error, leaves the already-closed source uncancelled, and
dispatches no further chunks to the sink
(`pipe_to_in_flight_write_error_during_close_flush_errors_pipe`).

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

The "abort does nothing after termination" family is split by which side terminated and where
the pipe is at the time. Covered: abort after the readable errored with a write still pending —
the readable's error, delivered on the pipe's next read, wins over the late abort, so `pipeTo`
rejects with it and `preventAbort` keeps the writable usable
(`pipe_to_late_abort_no_effect_after_readable_errored_with_pending_write`), plus the
no-pending-writes variant already covered.

Also covered: abort after the *writable* errored while the pipe is blocked awaiting a source read
(`pipe_to_late_abort_no_effect_after_writable_errored`). The pipe already selects `writer.closed()`
alongside the read, so it observes a destination error while reading — but this surfaced a
writable-task bug: controller messages were drained with a non-waking `try_next()`, so a
`controller.error()` arriving while the task was idle (no command, no in-flight write) sat
unprocessed and `closed()`/`ready()` never settled. The task now polls `ctrl_rx` with a registered
waker, fixing that whole class (also pinned directly, independent of piping, by
`controller_error_while_idle_rejects_closed_promptly`).

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

The mirror case — the *source* is already closed when the destination errors — must NOT cancel
the source: the last write rejects, `pipe_to` rejects with that write error, and the source's
`cancel()` is never invoked (cancelling a closed readable is a no-op; `preventCancel` is
irrelevant). Already correct — the readable's Cancel command short-circuits a `Closed` stream
without running the source's cancel steps — pinned by
`pipe_to_last_write_error_does_not_cancel_already_closed_source`.

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
hold. Also pinned: closing the writable with no queued chunks closes the readable
even under readable-side backpressure (explicit readable HWM 0), via
`closing_writable_closes_readable_under_backpressure` — the explicit-backpressure
complement to `closing_writable_closes_readable` (which relies on the spec-default HWM 0).

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

`cancel.any.js` tests 6/7 — where the transformer's `cancel()` calls `controller.error()` — are
covered (`cancel_calling_controller_error_rejects_cancel_and_parallel_close`,
`abort_calling_controller_error_rejects_abort_and_later_cancel`), expressible because
`TransformStreamDefaultController` is `Clone`, so a transformer captures it in `start()` and reaches
it from `cancel()` (which receives no controller). Two fixes made this work. (1) The cancel routing
reads the controller error back (`error_raised()`): a `cancel()` that returns `Ok` but errored the
controller now rejects with that error (spec `TransformStreamDefaultSourceCancelAlgorithm`: a
fulfilled cancel whose readable is now errored rejects with `readable.[[storedError]]`). (2) The
writable is errored *after* the cancel algorithm runs, not pre-errored with the reason — so a
`cancel()` that errors the controller leaves that error as the writable's stored error (the parallel
`writable.close()` rejects with it), while a clean cancel errors the writable with the reason
(`TransformStreamErrorWritableAndUnblockWrite`). The idiomatic `Err`-returning-`cancel()` outcome is
also covered by `cancel_that_throws_rejects_readable_cancel` / `abort_that_throws_rejects_writable_abort`.

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

`general.any.js` "desiredSize when closed/errored" and "read(), then error()" each surfaced a
divergence, now fixed. (1) A closed byte controller reported `desiredSize` `null`; the spec
distinguishes closed → `0` from errored → `null`, so `desired_size()` now collapses to `None`
only when errored (`byte_controller_desired_size_is_zero_when_closed`,
`byte_controller_desired_size_is_none_when_errored`). (2) A byte `read()` parked with no data
stranded forever when the source errored via `controller.error()` while its `pull()` still
returned `Ok` — the task drained parked reads only on enqueue/close/`pull()`-`Err`. It now
rejects them when the pull leaves the stream errored (`byte_parked_read_then_error_rejects`).

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

No portable spec behaviours are currently unmatched — the suite has no `#[ignore]`d divergences.

Previously divergent, now fixed: **an exception from `transform()` after `terminate()` errors the
readable with the thrown error** (WPT transform `errors.any.js`). The readable used to close
eagerly on `terminate()` even with a chunk still queued, so the following `error()` no-op'd. A
close whose queue is still draining now stays `readable` (closing) — the `Closed` transition is
deferred until a read empties the queue, and the controller's `error()` no longer gates on
`close_requested` — so the later `error()` applies. Pinned by
`transform_throw_after_terminate_errors_readable`.

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
- `error.any.js` — `controller.error()` errors the stream and surplus-error first-wins are both
  covered (see `write.any.js` above). `error()` on an already-errored stream is a no-op (the
  `ControllerMsg::Error` handler short-circuits); the closed-stream no-op variant is not
  separately pinned.
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

- readable `tee`: the coordinator's pull-scheduling bound ("pull only to fill the emptiest
  branch queue"; stop pulling once the source errors) — tied to the `BackpressureMode`
  extension's semantics.

### Stale test-comment labels (no coverage impact)

The piping test module uses section headers `error-propagation-via-abort.any.js` /
`error-propagation-via-cancel.any.js`; the real WPT files are `error-propagation-forward` /
`-backward`. Labels only.
