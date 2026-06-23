// WPT: streams/writable-streams/general.any.js

use crate::helpers::LifecycleSink;
use whatwg_streams::{StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

struct FailingStartSink;

impl WritableSink<u32> for FailingStartSink {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        Err("start rejected".into())
    }

    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        Ok(())
    }
}

struct FailingWriteSink;

impl WritableSink<u32> for FailingWriteSink {
    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        Err("sink write failed".into())
    }
}

// "WritableStream: initial state should be writable (not locked)"
#[cfg(feature = "send")]
#[tokio::test]
async fn initial_state_is_not_locked() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    assert!(!stream.locked());
}

// "WritableStream: get_writer() on an unlocked stream succeeds"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_succeeds_when_unlocked() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, _writer) = stream.get_writer().expect("get_writer should succeed");
    assert!(stream.locked());
}

// "WritableStream: get_writer() on an already-locked stream returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_fails_when_locked() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, _writer1) = stream.get_writer().unwrap();
    assert!(stream.get_writer().is_err());
}

// "WritableStream: dropping the writer releases the lock"
#[cfg(feature = "send")]
#[tokio::test]
async fn dropping_writer_releases_lock() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    {
        let (_locked, _writer) = stream.get_writer().unwrap();
        assert!(stream.locked());
    }
    assert!(!stream.locked());
}

// "WritableStream: if start() rejects, writes are rejected"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_rejection_rejects_writes() {
    let stream = WritableStream::builder(FailingStartSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert!(writer.write(1u32).await.is_err());
}

// ── WPT gaps: writable-streams/general.any.js ────────────────────────────────

// "desiredSize initial value"
// writer.desired_size() must equal HWM (default 1) when the queue is empty.
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_initial_value() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert_eq!(writer.desired_size(), Some(1), "desiredSize must equal HWM=1 initially");
}

// "desiredSize on a writer for a closed stream"
// After stream closes, desiredSize must be 0 (not null/None).
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_is_zero_on_closed_stream() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.close().await.unwrap();
    assert_eq!(
        writer.desired_size(),
        Some(0),
        "desiredSize must be 0 (not None) after stream is closed"
    );
}

// "ws.getWriter() on a closing WritableStream"
// After release_lock() (JS: writer.releaseLock()), getWriter() must succeed
// even when a close was previously initiated. The lock is tied to the writer's
// lifetime in Rust, so release_lock() is the idiomatic equivalent.
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_on_closing_stream_succeeds() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    // release_lock() explicitly frees the lock (matches JS writer.releaseLock())
    writer.release_lock().unwrap();
    assert!(
        stream.get_writer().is_ok(),
        "get_writer() must succeed after release_lock()"
    );
}

// "ws.getWriter() on a closed WritableStream"
// After stream closes and lock is released, getWriter() must succeed.
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_on_closed_stream_succeeds() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    {
        let (_locked, writer) = stream.get_writer().unwrap();
        writer.close().await.unwrap();
        // _locked drops → lock released
    }
    assert!(stream.get_writer().is_ok(), "get_writer() must succeed on an already-closed stream");
}

// "desiredSize on a writer for an errored stream"
// Spec: an errored stream's desiredSize is null. The Rust mapping is None
// (closed → Some(0), errored → None — the two terminal states are distinguishable).
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_is_none_on_errored_stream() {
    let stream = WritableStream::builder(FailingWriteSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert!(writer.write(1u32).await.is_err(), "the write should error the stream");
    assert_eq!(
        writer.desired_size(),
        None,
        "desiredSize must be None (JS null) on an errored stream"
    );
}

// "ws.getWriter() on an aborted WritableStream"
// Acquiring a writer is gated only on the lock, not on stream state: an aborted
// stream still yields a writer once the prior lock is freed. (The acquired writer's
// closed()/ready() then reject — that rejection is covered in abort.rs.)
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_on_aborted_stream_succeeds() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(Some("boom".into())).await.unwrap();
    writer.release_lock().unwrap();
    assert!(
        stream.get_writer().is_ok(),
        "get_writer() must succeed on an aborted stream once the lock is freed"
    );
}

// "ws.getWriter() on an errored WritableStream"
// Same gating as the aborted case: an errored stream still yields a writer.
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_on_errored_stream_succeeds() {
    let stream = WritableStream::builder(FailingWriteSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert!(writer.write(1u32).await.is_err(), "the write should error the stream");
    writer.release_lock().unwrap();
    assert!(
        stream.get_writer().is_ok(),
        "get_writer() must succeed on an errored stream once the lock is freed"
    );
}

// "the locked getter should return true if the stream has a writer"
// Positive counterpart to get_writer_fails_when_locked: locked() flips true the
// moment a writer is acquired.
#[cfg(feature = "send")]
#[tokio::test]
async fn locked_getter_true_when_writer_held() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    assert!(!stream.locked(), "fresh stream is unlocked");
    let (_locked, _writer) = stream.get_writer().unwrap();
    assert!(stream.locked(), "locked() must be true while a writer is held");
}

// Skipped from general.any.js — untranslatable to the Rust API, not coverage gaps:
//
// - "desiredSize on a released writer", "closed and ready on a released writer",
//   "ready promise should fire before closed on releaseLock", "redundant
//   releaseLock() is no-op": all observe a writer *after* its lock is released.
//   release_lock(self) consumes the writer by move, so a released writer is
//   unrepresentable — the stale-writer state these tests probe cannot be reached.
// - "call underlying sink methods as methods", "methods should not have .apply()
//   or .call() called": assert JS `this`-binding on sink callbacks. Sinks are Rust
//   trait impls; there is no receiver-rebinding to test.
// - "Subclassing WritableStream should work": JS prototype-chain subclassing has no
//   Rust analogue (composition/generics replace it).
