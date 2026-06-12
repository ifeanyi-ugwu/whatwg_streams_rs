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
