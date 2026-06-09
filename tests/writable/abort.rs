// WPT: streams/writable-streams/abort.any.js

use crate::helpers::LifecycleSink;
use whatwg_streams::{StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

struct FailingWriteSink;

impl WritableSink<u32> for FailingWriteSink {
    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        Err("write rejected".into())
    }
}

// "Aborting a WritableStream calls abort() on the underlying sink with the reason"
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_calls_sink_abort_with_reason() {
    let aborted = std::sync::Arc::new(std::sync::Mutex::new(None));
    let sink = LifecycleSink {
        aborted: aborted.clone(),
        ..Default::default()
    };
    let stream = WritableStream::builder(sink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(Some("abort reason".into())).await.unwrap();
    assert_eq!(aborted.lock().unwrap().as_deref(), Some("abort reason"));
}

// "Aborting a WritableStream immediately prevents further writes"
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_prevents_further_writes() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(None).await.unwrap();
    assert!(writer.write(1u32).await.is_err());
}

// "WritableStream: write() after abort() returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_after_abort_errors() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(Some("reason".into())).await.unwrap();
    assert!(writer.write(42u32).await.is_err());
}

// "WritableStream: close() after abort() returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn close_after_abort_errors() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(None).await.unwrap();
    assert!(writer.close().await.is_err());
}

// "WritableStream: if write() rejects, subsequent writes also fail"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_rejection_errors_stream() {
    let stream = WritableStream::builder(FailingWriteSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert!(writer.write(1u32).await.is_err());
    assert!(writer.write(2u32).await.is_err());
}

// ── WPT gaps (abort.any.js) ───────────────────────────────────────────────────

// "Aborting a WritableStream puts it in an errored state with the abort reason"
// WPT abort.any.js test 12
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_puts_stream_in_errored_state_with_reason() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(Some("abort reason".into())).await.unwrap();
    // After abort, writes must fail with the abort reason
    let err = writer.write(1u32).await.expect_err("write must reject after abort");
    assert!(
        err.to_string().contains("abort reason") || err.to_string().contains("Aborted"),
        "error must reflect abort reason, got: {err}"
    );
}

// "Aborting a WritableStream causes outstanding write() promises to be rejected with the reason"
// WPT abort.any.js test 13
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_rejects_outstanding_writes_with_reason() {
    use std::sync::Arc;
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct SlowWriteSink { unblock: Arc<tokio::sync::Notify> }
    impl WritableSink<u32> for SlowWriteSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.unblock.notified().await;
            Ok(())
        }
    }

    let stream = WritableStream::builder(SlowWriteSink { unblock: unblock2 })
        .strategy(whatwg_streams::CountQueuingStrategy::new(5))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let w = Arc::new(writer);

    // Enqueue a slow write — it blocks until notified
    let wc = w.clone();
    let write_fut = tokio::spawn(async move { wc.write(1u32).await });
    tokio::task::yield_now().await;

    // Spawn abort concurrently — abort() waits for the in-flight write's sink.write()
    // to complete before calling sink.abort(), so we must not await it before unblocking.
    let wa = w.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("abort reason".into())).await });
    tokio::task::yield_now().await;

    // Unblock the sink write — sink.write() completes, then sink.abort() runs
    unblock.notify_one();

    abort_fut.await.unwrap().unwrap();

    // The write completion was sent Err as soon as abort() was queued
    let write_result = write_fut.await.unwrap();
    assert!(write_result.is_err(), "in-flight write must reject when stream is aborted");
}

// "abort() should succeed even if sink.abort() is not supplied (no-op)"
// WPT abort.any.js test 17
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_succeeds_without_sink_abort_impl() {
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

    struct NoAbortSink;
    impl WritableSink<u32> for NoAbortSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
        // No abort() override — uses the default no-op
    }

    let stream = WritableStream::builder(NoAbortSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert!(writer.abort(None).await.is_ok(), "abort() must succeed when sink has no abort impl");
}

// "abort() should be idempotent: calling it twice fulfills both with Ok"
// WPT abort.any.js tests 47 and 48
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_is_idempotent() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(Some("first".into())).await.unwrap();
    // Second abort on an already-errored stream must also resolve (not reject)
    let second = writer.abort(Some("second".into())).await;
    assert!(second.is_ok(), "second abort() must fulfill per spec (got: {second:?})");
}

// "abort() on an already-errored stream should fulfill with undefined (Ok)"
// WPT abort.any.js test 49
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_on_errored_stream_fulfills() {
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};
    struct FailSink;
    impl WritableSink<u32> for FailSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            Err("write error".into())
        }
    }

    let stream = WritableStream::builder(FailSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    // Force stream into errored state
    let _ = writer.write(1u32).await;
    // Abort on an errored stream must fulfill with Ok
    assert!(writer.abort(None).await.is_ok(), "abort() on errored stream must fulfill");
}

// "abort() with no argument stores undefined as the error"
// WPT abort.any.js test 52
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_with_no_reason_succeeds() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    // abort(None) must succeed (not panic or error)
    writer.abort(None).await.unwrap();
    // Stream is errored — writes must fail
    assert!(writer.write(1u32).await.is_err());
}

// "underlying abort() should not be called until underlying write() completes"
// WPT abort.any.js test 24
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_abort_waits_for_inflight_write() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

    let write_done = Arc::new(Mutex::new(false));
    let abort_seen_write_done = Arc::new(Mutex::new(false));
    let write_done2 = write_done.clone();
    let abort_seen2 = abort_seen_write_done.clone();
    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct OrderSink {
        write_done: Arc<Mutex<bool>>,
        abort_seen: Arc<Mutex<bool>>,
        unblock: Arc<tokio::sync::Notify>,
    }

    impl WritableSink<u32> for OrderSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.unblock.notified().await;
            *self.write_done.lock().unwrap() = true;
            Ok(())
        }

        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.abort_seen.lock().unwrap() = *self.write_done.lock().unwrap();
            Ok(())
        }
    }

    let stream = WritableStream::builder(OrderSink {
        write_done: write_done2,
        abort_seen: abort_seen2,
        unblock: unblock2,
    })
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let w = Arc::new(writer);

    // Start slow write
    let wc = w.clone();
    tokio::spawn(async move { wc.write(1u32).await });
    tokio::task::yield_now().await;

    // Abort while write is in-flight
    let wa = w.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("reason".into())).await });
    tokio::task::yield_now().await;

    // Unblock write — write completes BEFORE abort is processed
    unblock.notify_one();
    abort_fut.await.unwrap().unwrap();

    assert!(
        *abort_seen_write_done.lock().unwrap(),
        "sink.abort() must only be called after the in-flight write completes"
    );
}
