// WPT: streams/writable-streams/write.any.js
// https://github.com/web-platform-tests/wpt/blob/master/streams/writable-streams/write.any.js

use crate::helpers::{CollectSink, FailAfterSink, LifecycleSink};
use whatwg_streams::{CountQueuingStrategy, StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

// ── Write ordering ────────────────────────────────────────────────────────────

// "WritableStream: writes are processed in the order they are enqueued"
#[cfg(feature = "send")]
#[tokio::test]
async fn writes_are_processed_in_order() {
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // Enqueue multiple writes and await each in sequence
    for i in 1u32..=5 {
        writer.write(i).await.unwrap();
    }
    writer.close().await.unwrap();

    assert_eq!(*collected.lock().unwrap(), vec![1, 2, 3, 4, 5]);
}

// "WritableStream: the sink receives chunks in the same order as write() calls"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_receives_chunks_in_enqueue_order() {
    // Use a slow sink to ensure ordering holds even with async processing
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let collected2 = collected.clone();

    struct OrderSink {
        collected: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl WritableSink<u32> for OrderSink {
        async fn write(
            &mut self,
            chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            // Yield so we're genuinely async
            tokio::task::yield_now().await;
            self.collected.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    let stream = WritableStream::builder(OrderSink { collected: collected2 })
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    writer.write(10u32).await.unwrap();
    writer.write(20u32).await.unwrap();
    writer.write(30u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(*collected.lock().unwrap(), vec![10, 20, 30]);
}

// ── Write promise semantics ───────────────────────────────────────────────────

// "WritableStream: write() promise resolves only after the sink's write() completes"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_promise_resolves_after_sink_write_completes() {
    use std::sync::{Arc, Mutex};

    let sink_completed = Arc::new(Mutex::new(Vec::<u32>::new()));
    let sink_completed2 = sink_completed.clone();

    struct TrackingSink {
        completed: Arc<Mutex<Vec<u32>>>,
    }

    impl WritableSink<u32> for TrackingSink {
        async fn write(
            &mut self,
            chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            tokio::task::yield_now().await; // simulate async I/O
            self.completed.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    let stream = WritableStream::builder(TrackingSink {
        completed: sink_completed2,
    })
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // write() resolves → sink.write() must have completed
    writer.write(42u32).await.unwrap();
    assert_eq!(
        *sink_completed.lock().unwrap(),
        vec![42],
        "write() should only resolve after the sink has processed the chunk"
    );
}

// "WritableStream: second write() begins only after the first sink write() finishes"
#[cfg(feature = "send")]
#[tokio::test]
async fn writes_are_serialized_through_sink() {
    use std::sync::{Arc, Mutex};

    // Sink records the ORDER in which writes START (not complete).
    // If writes were parallel, the order would be non-deterministic.
    let start_order = Arc::new(Mutex::new(Vec::<u32>::new()));
    let start_order2 = start_order.clone();

    struct SerialSink {
        start_order: Arc<Mutex<Vec<u32>>>,
    }

    impl WritableSink<u32> for SerialSink {
        async fn write(
            &mut self,
            chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.start_order.lock().unwrap().push(chunk);
            tokio::task::yield_now().await;
            Ok(())
        }
    }

    let stream = WritableStream::builder(SerialSink {
        start_order: start_order2,
    })
    .strategy(CountQueuingStrategy::new(10))
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    writer.write(1u32).await.unwrap();
    writer.write(2u32).await.unwrap();
    writer.write(3u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(
        *start_order.lock().unwrap(),
        vec![1, 2, 3],
        "sink.write() calls must be serialized — each starts only after the previous completes"
    );
}

// ── Write during close / error state ─────────────────────────────────────────

// "WritableStream: write() while a close is in flight returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_while_closing_returns_error() {
    use std::sync::Arc;

    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct SlowCloseSink {
        unblock: Arc<tokio::sync::Notify>,
    }

    impl WritableSink<u32> for SlowCloseSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            self.unblock.notified().await;
            Ok(())
        }
    }

    let stream = WritableStream::builder(SlowCloseSink { unblock: unblock2 })
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let writer = std::sync::Arc::new(writer);

    // Start close in a background task so it blocks on notify
    let w = writer.clone();
    let close_task = tokio::spawn(async move { w.close().await });

    // Give close command time to reach the task
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // write() should fail because close_requested is set
    let write_result = writer.write(1u32).await;
    assert!(
        write_result.is_err(),
        "write() while stream is closing must return an error"
    );

    // Unblock and let close complete cleanly
    unblock.notify_one();
    close_task.await.unwrap().unwrap();
}

// "WritableStream: enqueue() (fire-and-forget) on a closing stream is silently dropped"
#[cfg(feature = "send")]
#[tokio::test]
async fn enqueue_while_closing_is_dropped() {
    use std::sync::Arc;

    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();
    let write_count = Arc::new(std::sync::Mutex::new(0usize));
    let write_count2 = write_count.clone();

    struct CountingSink {
        write_count: Arc<std::sync::Mutex<usize>>,
        unblock: Arc<tokio::sync::Notify>,
    }

    impl WritableSink<u32> for CountingSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            *self.write_count.lock().unwrap() += 1;
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            self.unblock.notified().await;
            Ok(())
        }
    }

    let stream = WritableStream::builder(CountingSink {
        write_count: write_count2,
        unblock: unblock2,
    })
    .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let writer = std::sync::Arc::new(writer);

    let w = writer.clone();
    let close_task = tokio::spawn(async move { w.close().await });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // enqueue() on a closing stream — the implementation drops it silently
    let _ = writer.enqueue(99u32);

    unblock.notify_one();
    close_task.await.unwrap().unwrap();

    // The enqueued chunk should not have reached the sink
    assert_eq!(
        *write_count.lock().unwrap(),
        0,
        "enqueue() on a closing stream should be silently dropped"
    );
}

// "WritableStream: write() on an errored stream immediately returns the stored error"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_on_errored_stream_returns_stored_error() {
    use whatwg_streams::StreamError;

    struct FailingWriteSink;

    impl WritableSink<u32> for FailingWriteSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            Err(StreamError::from("sink write failed"))
        }
    }

    let stream = WritableStream::builder(FailingWriteSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // First write errors the stream
    assert!(writer.write(1u32).await.is_err());
    // Subsequent writes return the same error immediately
    assert!(writer.write(2u32).await.is_err());
    assert!(writer.write(3u32).await.is_err());
}

// "WritableStream: desired_size decreases as chunks are queued and recovers after writing"
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_tracks_queue_depth() {
    let stream = WritableStream::builder(LifecycleSink::default())
        .strategy(CountQueuingStrategy::new(3))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // Empty queue → desired_size == HWM
    assert_eq!(writer.desired_size(), Some(3));

    // After writing and letting it process, size returns to HWM
    writer.write(1u32).await.unwrap();
    assert_eq!(writer.desired_size(), Some(3), "desired_size should recover after write completes");
}

// ── WPT gaps: writable-streams/write.any.js ──────────────────────────────────

// "WritableStream should transition to waiting until write is acknowledged"
// desiredSize = HWM before write, drops to 0 while write is in-flight,
// returns to HWM once sink.write() completes.
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_tracks_queue_depth_with_inflight() {
    use std::sync::Arc;
    use whatwg_streams::{CountQueuingStrategy, StreamResult, WritableSink, WritableStreamDefaultController};

    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct BlockingSink { unblock: Arc<tokio::sync::Notify> }
    impl WritableSink<u32> for BlockingSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.unblock.notified().await;
            Ok(())
        }
    }

    let stream = WritableStream::builder(BlockingSink { unblock: unblock2 })
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // Before any write: desiredSize = HWM = 1
    assert_eq!(writer.desired_size(), Some(1), "desiredSize must start at HWM");

    let w = Arc::new(writer);
    let wc = w.clone();
    let write_fut = tokio::spawn(async move { wc.write(1u32).await });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // While write is in-flight: desiredSize = 0 (write is counted until sink.write() completes)
    assert_eq!(w.desired_size(), Some(0), "desiredSize must be 0 while write is in-flight");

    // Unblock sink — write completes, slot freed
    unblock.notify_one();
    write_fut.await.unwrap().unwrap();

    // After write completes: desiredSize = HWM = 1 again
    assert_eq!(w.desired_size(), Some(1), "desiredSize must restore to HWM after write completes");
}

// "WritableStream should complete asynchronous writes before close resolves"
// close() resolves only after every queued write has been drained through the sink.
// The sink yields inside write() to force a genuine async boundary — if close did
// not wait, the collected vector would be short when close() resolves.
#[cfg(feature = "send")]
#[tokio::test]
async fn close_waits_for_all_pending_writes() {
    use std::sync::{Arc, Mutex};

    let collected = Arc::new(Mutex::new(Vec::<u32>::new()));
    let collected2 = collected.clone();

    struct YieldSink {
        collected: Arc<Mutex<Vec<u32>>>,
    }
    impl WritableSink<u32> for YieldSink {
        async fn write(
            &mut self,
            chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            tokio::task::yield_now().await;
            self.collected.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    let stream = WritableStream::builder(YieldSink { collected: collected2 })
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // Queue five writes fire-and-forget, then close without awaiting them individually.
    for i in 1u32..=5 {
        writer.enqueue(i).unwrap();
    }
    writer.close().await.unwrap();

    assert_eq!(
        *collected.lock().unwrap(),
        vec![1, 2, 3, 4, 5],
        "close() must resolve only after all queued writes have drained"
    );
}

// "a large queue of writes should be processed completely"
// Stress the queue with a high HWM and many chunks; every one must reach the sink
// in order, and close() must wait for the whole backlog.
#[cfg(feature = "send")]
#[tokio::test]
async fn large_queue_processed_completely() {
    use std::sync::{Arc, Mutex};

    let collected = Arc::new(Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(1000))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    for i in 0u32..1000 {
        writer.enqueue(i).unwrap();
    }
    writer.close().await.unwrap();

    let got = collected.lock().unwrap();
    assert_eq!(got.len(), 1000, "every queued chunk must reach the sink");
    assert_eq!(got.first(), Some(&0));
    assert_eq!(got.last(), Some(&999));
    assert!(got.iter().copied().eq(0u32..1000), "chunks must arrive in enqueue order");
}

// "when write returns a rejected promise, queued writes and close should be cleared"
// A write queued behind a failing write must itself reject, and a pending close()
// must reject too — a sink error tears down the whole backlog, not just the chunk
// that failed.
#[cfg(feature = "send")]
#[tokio::test]
async fn write_rejection_clears_pending_queue_and_close() {
    let sink = FailAfterSink {
        fail_after: 0, // the very first sink.write() rejects
        count: Default::default(),
    };
    let stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // Chunk 1 is queued first (ordered command channel) and will fail in the sink.
    writer.enqueue(1u32).unwrap();

    // Chunk 2 is queued behind it; its write() future must reject — not hang —
    // once chunk 1 errors the stream. The timeout turns a regression (queued
    // completions never sent) into a fast failure instead of a hung suite.
    let timeout = std::time::Duration::from_secs(5);
    let queued_write = tokio::time::timeout(timeout, writer.write(2u32))
        .await
        .expect("write queued behind a failing write must resolve, not hang");
    assert!(queued_write.is_err(), "a write queued behind a failing write must be rejected");

    let pending_close = tokio::time::timeout(timeout, writer.close())
        .await
        .expect("close behind a failing write must resolve, not hang");
    assert!(pending_close.is_err(), "a pending close must reject when an earlier write fails");
}

// WPT: write.any.js —
// "writer.write(), ready and closed reject with the error passed to controller.error()
//  made before sink.write rejection"
// The write() promise carries the sink's own returned error, but the stream's stored
// error (ready/closed) is the earlier controller.error() value — the first error wins.
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_error_before_write_rejection_wins_stream_error() {
    struct ErrThenReturnErrSink;
    impl WritableSink<u32> for ErrThenReturnErrSink {
        async fn write(
            &mut self,
            _chunk: u32,
            controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            controller.error("error1".into());
            Err("error2".into())
        }
    }

    let stream = WritableStream::builder(ErrThenReturnErrSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    let write_err = writer.write(1u32).await.expect_err("write() must reject");
    assert!(
        write_err.to_string().contains("error2"),
        "write() rejects with the sink's returned error, got: {write_err}"
    );

    let closed_err = writer.closed().await.expect_err("closed() must reject on the errored stream");
    assert!(
        closed_err.to_string().contains("error1"),
        "the stream's stored error is the controller.error() value (first error wins), got: {closed_err}"
    );
}

// WPT: error.any.js — "surplus calls to controller.error() should be a no-op"
// The first error wins; a second controller.error() does not overwrite the stored error.
//
// KNOWN DIVERGENCE (ignored): the ControllerMsg::Error handler overwrites the stored
// error unconditionally, so a second controller.error() wins ("last wins" instead of
// "first wins"). A naive first-wins guard regresses the controller.error()-then-write-
// rejection case (which is currently spec-correct), because both error paths share the
// handler and depend on async message ordering. A correct fix must make stored_error
// first-wins across both the controller and write-rejection paths while each write
// promise still carries its own error. Documented in WPT_COVERAGE.md.
#[cfg(feature = "send")]
#[tokio::test]
#[ignore = "known divergence: surplus controller.error() overwrites (last-wins) instead of first-wins; correct fix needs cross-path error-precedence rework (see WPT_COVERAGE.md)"]
async fn surplus_controller_error_is_noop_first_wins() {
    struct DoubleErrorSink;
    impl WritableSink<u32> for DoubleErrorSink {
        async fn write(
            &mut self,
            _chunk: u32,
            controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            controller.error("first".into());
            controller.error("second".into());
            Ok(())
        }
    }

    let stream = WritableStream::builder(DoubleErrorSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let _ = writer.write(1u32).await;

    let closed_err = writer.closed().await.expect_err("closed() must reject");
    assert!(
        closed_err.to_string().contains("first"),
        "the first controller.error() wins; the surplus call is a no-op, got: {closed_err}"
    );
}

// Skipped from write.any.js — untranslatable to the Rust API, not coverage gaps:
//
// - "writing to a released writer should reject": release_lock(self) consumes the
//   writer by move, so calling write() on a released writer is a compile error, not
//   a runtime rejection — the state is unrepresentable.
// - "returning a thenable from write() should work": JS awaits any object with a
//   .then method. A Rust sink's write() returns a concrete Future bound by the
//   trait; there is no thenable duck-typing to exercise.
// - "WritableStreamDefaultWriter should work when manually constructed", "failing
//   DefaultWriter constructor should not release an existing writer": writers are
//   obtained only via get_writer(); there is no public constructor to misuse.
