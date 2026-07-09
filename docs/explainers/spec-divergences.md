# Deliberate divergences from the WHATWG Streams spec

This library reproduces the spec's **behaviour**, not JavaScript's **idioms** — that
distinction, and the long list of idiom-only WPT tests skipped because of it, lives in
[`WPT_COVERAGE.md`](../../crates/whatwg_streams/tests/WPT_COVERAGE.md). In three places it goes
further and *deliberately* differs from a literal reading of the spec, because there the spec's own
behaviour is either **worse for users** or **unreachable** through a Rust API. Each is a considered
choice, pinned by a test. The ledger carries a one-line conformance entry for each; the reasoning
lives here.

## 1. Teeing: cancelling one branch resolves now, not "when both cancel"

`tee()` splits one source into two **branches** that read the same data independently. Cancelling a
branch means "I'm done with *this copy*." The key fact:

> Cancelling one branch does **not** cancel the source. The other branch still wants the data. The
> source is only cancelled once **both** branches give up.

So there are two separate events:

| event | when | can it fail? |
| --- | --- | --- |
| **detach a branch** | immediately, on `branch.cancel()` | no — you just stop feeding it |
| **cancel the shared source** | later, only once *both* branches cancel | yes — `source.cancel()` can throw |

The spec ties `branch.cancel()`'s promise to the **second** event (both branches share one composite
cancel promise). Two consequences follow, and both hurt the common case:

```js
// SPEC:
const [a, b] = readable.tee();
await a.getReader().cancel();   // ⚠️ HANGS FOREVER if you keep reading b
//   ...a.cancel()'s promise waits for the source cancel, which waits for b to cancel too.
```

- Cancelling **one** branch and moving on — the single most common tee usage — **hangs**.
- A throwing `source.cancel()` rejects **both** branches.

This implementation keeps the two events separate. The **first** branch to cancel resolves `Ok`
immediately — an honest statement that *its branch* is detached, which claims nothing about the
source (one-branch cancel never reaches it). Only the **second** branch, which actually triggers
`source.cancel()`, reflects its result:

```rust
let (a, b) = readable.tee();
a_reader.cancel(None).await;    // Ok immediately — branch a detached, keep reading b
// ... if both cancel and source.cancel() throws:
//     first-to-cancel  → Ok    (its branch detached: true)
//     second-to-cancel → Err   (it ran source.cancel(), which threw)
```

**The one rough edge:** on a *both-branches* cancel with a throwing `source.cancel()`, the failure
surfaces on whichever branch cancels **last**, not symmetrically ("last one out reports whether
turning off the lights failed"). A deterministic error path would be pure polish.

**Verdict:** this is the better behaviour for users. The spec's composite promise conflates "detach a
branch" with "clean up the source," and the price is a hang in the common case. Adopting strict
fidelity would reintroduce that hang. Pinned exactly (not with a loose `is_err() || is_err()`) by
`tee_failing_source_cancel_propagates_to_branch_cancel`.

## 2. Piping: which error wins when a broken source meets a broken destination

Both sides are already broken — source errored with `error1`, destination broken with `error2`. You
do `source.pipeTo(dest)`. **Which error does the pipe reject with?** The spec's answer hinges on a
writable-state distinction this implementation does not model:

- **`errored`** — the writable has *finished* erroring; its error is locked in.
- **`erroring`** — still *transitioning* into the error (e.g. `controller.error()` was called inside
  `start()`, before the controller has "started"). Not done yet.

|                        | dest is `erroring` (errored inside `start()`) | dest is `errored` (fully done) |
| ---------------------- | --------------------------------------------- | ------------------------------ |
| Spec rejects with      | `error2` (the dest's own)                     | `error1` (the source's)        |
| This impl rejects with | `error1` — wrong                              | `error1` — correct             |

**Why the spec flips:** aborting a writable that is still `erroring` *discards* the reason passed to
the abort and finalizes with the writable's **own** pending error (`error2`); aborting a fully
`errored` one is a no-op, so the pipe falls back to the source error (`error1`).

**Why this impl always gives `error1`:** the writable has only `Writable` / `Closed` / `Errored` — no
transitional `erroring`. So `writer.ready()` returns `Err(error2)`, the pipe then cancels the errored
source (`reader.cancel()` → `Err(error1)`), and `error1` surfaces in **both** columns: right for
`errored`, wrong for `erroring`.

**Why it barely matters:** reaching the wrong cell needs a writable that errors *synchronously in
`start()`* **and** an already-errored source piped in the same tick — piping one broken stream into
another and disputing whose error label wins. No real program does this. The fix would be modelling
transitional `erroring` (and `closing`) writable states — not worth it for a cell nobody reaches, so
it is left as a documented divergence.

## 3. Piping: an empty source into a closed destination errors, it doesn't shrug

Think of `pipe_to` as plumbing: a **source** is a faucet (data comes out), a **destination** is a
bucket (data goes in). It reads from the faucet and pours into the bucket until the faucet runs dry,
then seals the bucket.

- **Empty source** = a faucet already off. Read it and it says "done, nothing here."
  (`ReadableStream::from_vec(Vec::<u32>::new())`)
- **Closed destination** = a bucket already sealed shut. You cannot pour into it. (`writer.close()`
  up front.)

```
     empty faucet  ──pipe_to──▶  sealed bucket
     (nothing to pour)           (can't accept anything)

         succeed (nothing happened), or fail (invalid target)?
```

| | reasoning | verdict |
| --- | --- | --- |
| **WHATWG spec / WPT** | "The faucet was empty, so I never needed the bucket." | **fulfil** |
| **This impl** | "You told me to pipe into a *sealed* bucket — an invalid target, whatever the faucet held." | **reject** |

**The mechanic:** `pipe_to` grabs a writer on the destination and checks its state before doing
anything useful. The destination is already closed, so it bails immediately with *"cannot pipe to a
closed writable stream"* — the same rule as `pipe_to_closed_dest_cancels_source_and_rejects`, with no
exception carved out for "…but the source happened to be empty."

**On determinism** (an audit worried this was luck): internally `pipe_to` runs a `select!` racing
"source produced / is done" against "destination closed" — and here **both** are true at once. The
fear was a coin flip: sometimes fulfil, sometimes reject. It is not — it rejects **every time**. So
this is a fixed, predictable difference from WPT, not a race. That determinism is the whole point of
the pinning test, `pipe_to_empty_source_into_closed_dest_rejects`.

**Verdict:** the louder behaviour is arguably the more helpful one. Piping into an already-closed
destination is almost always a programming mistake; this impl surfaces it as an error rather than
silently reporting success because the source happened to be empty.
