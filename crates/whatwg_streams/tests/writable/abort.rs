// WPT: streams/writable-streams/abort.any.js

use crate::helpers::LifecycleSink;
use whatwg_streams::{StreamError, StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

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

// WPT: abort.any.js — "Aborting a WritableStream prevents further writes after any that are in
// progress". A write already dispatched to the sink finishes with its own result; writes still
// queued behind it are rejected with the abort reason.
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_rejects_queued_writes_but_in_flight_finishes() {
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

    // write(1) is dispatched to the sink and blocks there — it is the in-flight write.
    let w1 = w.clone();
    let write1 = tokio::spawn(async move { w1.write(1u32).await });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // write(2) is queued behind the in-flight write(1).
    let w2 = w.clone();
    let write2 = tokio::spawn(async move { w2.write(2u32).await });
    tokio::task::yield_now().await;

    // Abort with a reason. abort() waits for the in-flight write to settle before sink.abort().
    let wa = w.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("abort reason".into())).await });
    tokio::task::yield_now().await;

    // Let the in-flight write's sink.write() complete (Ok), then sink.abort() runs.
    unblock.notify_one();
    abort_fut.await.unwrap().unwrap();

    // The in-flight write finishes with its own result; the queued write rejects with the reason.
    assert!(
        write1.await.unwrap().is_ok(),
        "the in-flight write finishes with its own (successful) result, not the abort error"
    );
    let queued = write2.await.unwrap().expect_err("a queued write must reject on abort");
    assert!(
        queued.to_string().contains("abort reason"),
        "the queued write rejects with the abort reason, got: {queued}"
    );
}

// "a sink interrupted mid-write can read the abort reason from the controller"
// abort_future()/with_abort() tell a sink *that* it was aborted; abort_reason()
// tells it *why*. A sink parked on abort_future() during an in-flight write must
// see the reason passed to writer.abort().
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_reads_abort_reason_during_in_flight_write() {
    use std::sync::{Arc, Mutex};

    struct ReasonObservingSink {
        observed: Arc<Mutex<Option<String>>>,
    }
    impl WritableSink<u32> for ReasonObservingSink {
        async fn write(
            &mut self,
            _chunk: u32,
            controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            controller.abort_future().await;
            let reason = controller.abort_reason();
            *self.observed.lock().unwrap() = reason.clone();
            Err(StreamError::Aborted(reason))
        }
    }

    let observed = Arc::new(Mutex::new(None));
    let stream = WritableStream::builder(ReasonObservingSink {
        observed: observed.clone(),
    })
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // Enqueue a write; it parks in the sink on abort_future(). Hold the future so
    // its completion channel stays open until abort resolves it.
    let _writing = writer.write(1u32);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    writer.abort(Some("mid-write reason".into())).await.unwrap();

    assert_eq!(
        observed.lock().unwrap().as_deref(),
        Some("mid-write reason"),
        "sink must observe the abort reason via controller.abort_reason()"
    );
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

// ── WPT gaps: aborting.any.js ─────────────────────────────────────────────────

// "WritableStream: if sink's abort throws, the promise returned by writer.abort() rejects"
// WPT aborting.any.js test 7
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_abort_throws_rejects_abort_promise() {
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};
    struct ThrowingAbortSink;
    impl WritableSink<u32> for ThrowingAbortSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err(StreamError::from("abort threw"))
        }
    }

    let stream = WritableStream::builder(ThrowingAbortSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let result = writer.abort(Some("reason".into())).await;
    assert!(result.is_err(), "abort() must reject when sink.abort() throws");
    assert!(
        result.unwrap_err().to_string().contains("abort threw"),
        "rejection must carry the sink abort error"
    );
}

// "underlying abort() should not be called if underlying close() has started"
// WPT aborting.any.js test 25 — validates our abort-during-close fix.
// When close is in-flight, abort() resolves but sink.abort() is NOT invoked.
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_abort_not_called_when_close_already_started() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

    let abort_called = Arc::new(Mutex::new(false));
    let abort_called2 = abort_called.clone();
    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct TrackedCloseSink {
        abort_called: Arc<Mutex<bool>>,
        unblock: Arc<tokio::sync::Notify>,
    }
    impl WritableSink<u32> for TrackedCloseSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
        async fn close(self) -> StreamResult<()> {
            self.unblock.notified().await;
            Ok(())
        }
        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.abort_called.lock().unwrap() = true;
            Ok(())
        }
    }

    let stream = WritableStream::builder(TrackedCloseSink {
        abort_called: abort_called2,
        unblock: unblock2,
    }).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let w = Arc::new(writer);

    // Start slow close — inflight
    let wc = w.clone();
    let close_fut = tokio::spawn(async move { wc.close().await });
    tokio::task::yield_now().await;

    // Abort while close is inflight — per spec, sink.abort() must NOT be called
    let wa = w.clone();
    let abort_result = tokio::spawn(async move { wa.abort(Some("reason".into())).await });
    tokio::task::yield_now().await;

    // Unblock close
    unblock.notify_one();
    abort_result.await.unwrap().unwrap(); // abort must resolve Ok
    let _ = close_fut.await;

    assert!(
        !*abort_called.lock().unwrap(),
        "sink.abort() must NOT be called when sink.close() was already started"
    );
}

// "an abort() that happens during a write() should trigger underlying abort() even with a close() queued"
// WPT aborting.any.js test 27
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_during_write_with_queued_close_still_calls_sink_abort() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::{CountQueuingStrategy, StreamResult, WritableSink, WritableStreamDefaultController};

    let abort_called = Arc::new(Mutex::new(false));
    let abort_called2 = abort_called.clone();
    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct TrackAbortSink {
        abort_called: Arc<Mutex<bool>>,
        unblock: Arc<tokio::sync::Notify>,
    }
    impl WritableSink<u32> for TrackAbortSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.unblock.notified().await;
            Ok(())
        }
        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.abort_called.lock().unwrap() = true;
            Ok(())
        }
    }

    let stream = WritableStream::builder(TrackAbortSink {
        abort_called: abort_called2,
        unblock: unblock2,
    })
    .strategy(CountQueuingStrategy::new(4))
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let w = Arc::new(writer);

    // Start a slow write (will block), queue a close, then abort
    let wc = w.clone();
    tokio::spawn(async move { wc.write(1u32).await });
    tokio::task::yield_now().await;

    let wc = w.clone();
    tokio::spawn(async move { wc.close().await });
    tokio::task::yield_now().await;

    let wa = w.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("abort".into())).await });
    tokio::task::yield_now().await;

    // Unblock write — after write completes the task should process abort
    unblock.notify_one();
    abort_fut.await.unwrap().unwrap(); // abort resolves Ok

    assert!(
        *abort_called.lock().unwrap(),
        "sink.abort() must be called when abort fires during an in-flight write (even with close queued)"
    );
}

// "sink abort() should not be called until sink start() is done"
// WPT aborting.any.js test 38
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_abort_waits_for_slow_start() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

    let start_done = Arc::new(Mutex::new(false));
    let abort_saw_start_done = Arc::new(Mutex::new(false));
    let start_done2 = start_done.clone();
    let abort_saw2 = abort_saw_start_done.clone();
    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct SlowStartSink {
        start_done: Arc<Mutex<bool>>,
        abort_saw: Arc<Mutex<bool>>,
        unblock: Arc<tokio::sync::Notify>,
    }
    impl WritableSink<u32> for SlowStartSink {
        async fn start(&mut self, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.unblock.notified().await;
            *self.start_done.lock().unwrap() = true;
            Ok(())
        }
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.abort_saw.lock().unwrap() = *self.start_done.lock().unwrap();
            Ok(())
        }
    }

    let stream = WritableStream::builder(SlowStartSink {
        start_done: start_done2,
        abort_saw: abort_saw2,
        unblock: unblock2,
    }).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let w = Arc::new(writer);

    let wa = w.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("reason".into())).await });
    tokio::task::yield_now().await;

    // Unblock start — abort must run AFTER start finishes
    unblock.notify_one();
    abort_fut.await.unwrap().unwrap();

    assert!(
        *abort_saw_start_done.lock().unwrap(),
        "sink.abort() must only be called after sink.start() completes"
    );
}

// "stream abort() promise should still resolve if sink start() rejects"
// WPT aborting.any.js test 40
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_resolves_even_when_start_rejects() {
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};
    struct FailStartSink;
    impl WritableSink<u32> for FailStartSink {
        async fn start(&mut self, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            Err(StreamError::from("start failed"))
        }
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
    }

    let stream = WritableStream::builder(FailStartSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    // Even though start() will reject, abort() must resolve (not reject)
    let result = writer.abort(None).await;
    assert!(result.is_ok(), "abort() must resolve even when sink.start() rejects, got: {result:?}");
}

// "abort() should succeed despite rejection from write"
// WPT aborting.any.js test 43
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_succeeds_despite_write_rejection() {
    use std::sync::Arc;
    use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct FailWriteSink { unblock: Arc<tokio::sync::Notify> }
    impl WritableSink<u32> for FailWriteSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.unblock.notified().await;
            Err(StreamError::from("write failed"))
        }
    }

    let stream = WritableStream::builder(FailWriteSink { unblock: unblock2 })
        .strategy(whatwg_streams::CountQueuingStrategy::new(4))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let w = Arc::new(writer);

    let wc = w.clone();
    let write_fut = tokio::spawn(async move { wc.write(1u32).await });
    tokio::task::yield_now().await;

    // Abort while write is in-flight
    let wa = w.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("abort".into())).await });
    tokio::task::yield_now().await;

    // Unblock the failing write
    unblock.notify_one();
    let write_result = write_fut.await.unwrap();
    let abort_result = abort_fut.await.unwrap();

    // Write must have failed, abort must still succeed
    assert!(write_result.is_err(), "write must reject");
    assert!(abort_result.is_ok(), "abort must succeed despite write rejection, got: {abort_result:?}");
}

// WPT: aborting.any.js —
// "Aborting a WritableStream before it starts should cause the writer's unsettled
//  ready promise to reject"
// The promise-identity assertion (writer.ready === readyPromise) is a JS idiom (§1);
// the behavioural core reproduced here is that the pending ready() and the pending
// write() both reject when abort() fires before start() has finished.
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_before_start_rejects_pending_ready_and_write() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    struct SlowStartSink {
        release: Arc<Notify>,
        wrote: Arc<AtomicBool>,
    }

    impl WritableSink<u32> for SlowStartSink {
        async fn start(&mut self, _c: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.release.notified().await;
            Ok(())
        }
        async fn write(
            &mut self,
            _chunk: u32,
            _c: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.wrote.store(true, Ordering::Release);
            Ok(())
        }
    }

    let release = Arc::new(Notify::new());
    let wrote = Arc::new(AtomicBool::new(false));
    // HWM 0 backpressures the stream from the start, so ready() is genuinely pending
    // (rather than resolving Ok before the async write commits).
    let stream = WritableStream::builder(SlowStartSink {
        release: release.clone(),
        wrote: wrote.clone(),
    })
    .strategy(whatwg_streams::CountQueuingStrategy::new(0))
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let writer = Arc::new(writer);

    // A write and a ready() wait, both pending while start() is still blocked.
    let w1 = writer.clone();
    let write_fut = tokio::spawn(async move { w1.write(1u32).await });
    let w2 = writer.clone();
    let ready_fut = tokio::spawn(async move { w2.ready().await });
    let wa = writer.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("error1".into())).await });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Unblock start(); abort now proceeds and errors the stream.
    release.notify_one();

    let abort_result = abort_fut.await.unwrap();
    let write_result = write_fut.await.unwrap();
    let ready_result = ready_fut.await.unwrap();

    assert!(abort_result.is_ok(), "abort() must resolve: {abort_result:?}");
    assert!(
        write_result.is_err(),
        "the pending write() must reject when the stream is aborted before start()"
    );
    assert!(
        ready_result.is_err(),
        "the pending ready() must reject when the stream is aborted before start()"
    );
    assert!(
        !wrote.load(Ordering::Acquire),
        "sink write() must not be called for a write aborted before start()"
    );
}

// WPT: aborting.any.js — ".closed should not resolve before rejected write(); write() error
// should not overwrite abort() error". A write in flight rejects with its own error while an
// abort(reason) is pending: the write() promise carries the write's error, while closed() rejects
// with the abort reason — first-wins, so the later write rejection must not overwrite it.
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_error_not_overwritten_by_later_write_rejection() {
    use futures::FutureExt;
    use std::sync::Arc;

    struct BlockThenRejectSink {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl WritableSink<u32> for BlockThenRejectSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.started.notify_one();
            self.release.notified().await;
            Err("error1".into())
        }
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let stream = WritableStream::builder(BlockThenRejectSink {
        started: started.clone(),
        release: release.clone(),
    })
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.ready().await.unwrap();

    let write_fut = writer.write(1u32);
    let orchestrate = async {
        started.notified().await; // the sink's write() is in flight
        let abort_fut = writer.abort(Some("error2".to_string()));
        futures::pin_mut!(abort_fut);
        // Send the abort command while the write is still in flight, then let the write finish.
        let _ = futures::poll!(&mut abort_fut);
        release.notify_one();
        abort_fut.await
    };
    let (write_res, abort_res) = futures::join!(write_fut, orchestrate);

    let write_err = write_res.expect_err("write() must reject");
    assert!(
        write_err.to_string().contains("error1"),
        "write() carries its own error, got: {write_err}"
    );
    abort_res.expect("abort() must fulfill");

    let closed_err = writer.closed().await.expect_err("closed() must reject");
    assert!(
        closed_err.to_string().contains("error2"),
        "closed() rejects with the abort reason, not the later write error, got: {closed_err}"
    );
}
