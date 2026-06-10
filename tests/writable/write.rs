// WPT: streams/writable-streams/write.any.js
// https://github.com/web-platform-tests/wpt/blob/master/streams/writable-streams/write.any.js

use crate::helpers::{CollectSink, LifecycleSink, SlowSink};
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
