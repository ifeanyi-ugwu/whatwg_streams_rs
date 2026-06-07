// WPT: streams/transform-streams/

use whatwg_streams::{
    StreamResult, TransformStream, TransformStreamDefaultController, Transformer,
};

struct DoubleT;

impl Transformer<u32, u32> for DoubleT {
    async fn transform(
        &mut self,
        chunk: u32,
        controller: &mut TransformStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk * 2)
    }
}

struct UppercaseT;

impl Transformer<String, String> for UppercaseT {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk.to_uppercase())
    }
}

struct FilterOddT;

impl Transformer<i32, i32> for FilterOddT {
    async fn transform(
        &mut self,
        chunk: i32,
        controller: &mut TransformStreamDefaultController<i32>,
    ) -> StreamResult<()> {
        if chunk % 2 != 0 {
            controller.enqueue(chunk)?;
        }
        Ok(())
    }
}

struct ErrorOnThreeT;

impl Transformer<i32, i32> for ErrorOnThreeT {
    async fn transform(
        &mut self,
        chunk: i32,
        controller: &mut TransformStreamDefaultController<i32>,
    ) -> StreamResult<()> {
        if chunk == 3 {
            Err("cannot process 3".into())
        } else {
            controller.enqueue(chunk)
        }
    }
}

// WPT: transform-streams/general.any.js —
// "TransformStream: enqueued chunks appear on the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_passes_chunks_through() {
    use whatwg_streams::CountQueuingStrategy;
    // HWM=10 so sequential writes don't block on backpressure (not testing bp here)
    let ts = TransformStream::builder(DoubleT)
        .readable_strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    writer.write(2u32).await.unwrap();
    writer.write(3u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(2));
    assert_eq!(reader.read().await.unwrap(), Some(4));
    assert_eq!(reader.read().await.unwrap(), Some(6));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "TransformStream: transform can change the type of chunks"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_changes_chunk_type() {
    use whatwg_streams::CountQueuingStrategy;
    let ts = TransformStream::builder(UppercaseT)
        .readable_strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write("hello".to_string()).await.unwrap();
    writer.write("world".to_string()).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some("HELLO".to_string()));
    assert_eq!(reader.read().await.unwrap(), Some("WORLD".to_string()));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "TransformStream: transform can filter out chunks"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_can_filter_chunks() {
    use whatwg_streams::CountQueuingStrategy;
    let ts = TransformStream::builder(FilterOddT)
        .readable_strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    for i in 1i32..=5 {
        writer.write(i).await.unwrap();
    }
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(1));
    assert_eq!(reader.read().await.unwrap(), Some(3));
    assert_eq!(reader.read().await.unwrap(), Some(5));
    assert_eq!(reader.read().await.unwrap(), None);
}

// WPT: transform-streams/general.any.js —
// "TransformStream: closing the writable side closes the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn closing_writable_closes_readable() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.close().await.unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
}

// WPT: transform-streams/errors.any.js —
// "TransformStream: error in transform() errors both sides"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_error_errors_both_sides() {
    let ts = TransformStream::builder(ErrorOnThreeT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // JS WPT fires all writes and reads concurrently via Promise.all(). With HWM=1,
    // write(2) blocks until chunk 1 is consumed, so the two sides must run concurrently.
    let write_side = async {
        writer.write(1i32).await.unwrap();
        writer.write(2i32).await.unwrap();
        assert!(writer.write(3i32).await.is_err());
    };
    let read_side = async {
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert!(reader.read().await.is_err());
    };
    tokio::join!(write_side, read_side);
}

// WPT: transform-streams/general.any.js —
// "TransformStream: abort() on the writable errors the readable"
// Per spec: abort() itself resolves (Ok) — it's the readable that becomes errored.
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_writable_errors_readable() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(2));

    // abort() itself resolves with Ok — it's the readable side that errors.
    let abort_result = writer.abort(Some("aborted".into())).await;
    assert!(abort_result.is_ok(), "abort() must resolve per spec, got: {abort_result:?}");

    // The readable side is now errored
    assert!(reader.read().await.is_err());
}

// WPT: transform-streams/flush.any.js —
// "TransformStream: flush() is called when the writable closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn flush_called_on_writable_close() {
    use std::sync::{Arc, Mutex};

    struct FlushTracker {
        flushed: Arc<Mutex<bool>>,
    }

    impl Transformer<u32, u32> for FlushTracker {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn flush(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.flushed.lock().unwrap() = true;
            controller.enqueue(999u32)?; // sentinel chunk emitted in flush
            Ok(())
        }
    }

    let flushed = Arc::new(Mutex::new(false));
    let ts = TransformStream::builder(FlushTracker {
        flushed: flushed.clone(),
    })
    .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(1));
    assert_eq!(reader.read().await.unwrap(), Some(999)); // flush sentinel
    assert_eq!(reader.read().await.unwrap(), None);
    assert!(*flushed.lock().unwrap());
}

// ── WPT: transform-streams/terminate.any.js ──────────────────────────────────
// https://github.com/web-platform-tests/wpt/blob/master/streams/transform-streams/terminate.any.js

// "TransformStream: controller.terminate() closes the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_closes_readable_side() {
    struct TerminatingT;

    impl Transformer<u32, u32> for TerminatingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)?;
            controller.terminate()?; // close readable, error writable
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminatingT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();

    // First read returns the enqueued chunk
    assert_eq!(reader.read().await.unwrap(), Some(1));
    // Second read returns None — readable is closed
    assert_eq!(reader.read().await.unwrap(), None);
}

// "TransformStream: controller.terminate() errors the writable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_errors_writable_side() {
    struct TerminatingT;

    impl Transformer<u32, u32> for TerminatingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)?;
            controller.terminate()?;
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminatingT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // First write triggers terminate() — readable gets the chunk, writable errors
    writer.write(1u32).await.unwrap();
    let _ = reader.read().await; // consume the chunk

    // Subsequent writes on the writable should now fail
    assert!(
        writer.write(2u32).await.is_err(),
        "write() after terminate() must return an error"
    );
}

// "TransformStream: controller.terminate() called in start() closes immediately"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_in_start_closes_immediately() {
    struct TerminateOnStartT;

    impl Transformer<u32, u32> for TerminateOnStartT {
        async fn start(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.terminate()?;
            Ok(())
        }

        async fn transform(
            &mut self,
            _chunk: u32,
            _controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminateOnStartT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // Readable is already closed — reads return None immediately
    assert_eq!(reader.read().await.unwrap(), None);
    // Writable is errored — writes fail
    assert!(writer.write(1u32).await.is_err());
}

// "TransformStream: terminate() in flush() closes the readable after all chunks"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_in_flush_closes_after_chunks() {
    struct TerminateInFlushT;

    impl Transformer<u32, u32> for TerminateInFlushT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn flush(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // terminate() in flush: readable closes, no sentinel chunk needed
            controller.terminate()?;
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminateInFlushT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // JS WPT uses Promise.all([write1, write2, close, read1, read2, read3]) — all
    // concurrent.  With HWM=1 write(2) blocks until chunk 1 is consumed, so the
    // write and read sides must run concurrently, matching the spec's intent.
    let write_and_close = async {
        writer.write(1u32).await.unwrap();
        writer.write(2u32).await.unwrap();
        writer.close().await.unwrap();
    };
    let read_all = async {
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), None);
    };
    tokio::join!(write_and_close, read_all);
}

// "TransformStream: desired_size on the controller reflects the readable side's HWM"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_desired_size_reflects_readable_hwm() {
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));
    let captured2 = captured.clone();

    struct CapturingT {
        captured: Arc<Mutex<Option<isize>>>,
    }

    impl Transformer<u32, u32> for CapturingT {
        async fn transform(
            &mut self,
            _chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.captured.lock().unwrap() = controller.desired_size();
            controller.terminate()?;
            Ok(())
        }
    }

    use whatwg_streams::CountQueuingStrategy;
    let ts = TransformStream::builder(CapturingT { captured: captured2 })
        .readable_strategy(CountQueuingStrategy::new(4))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, _reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    tokio::task::yield_now().await;

    // Readable HWM=4, queue was empty when transform ran → desired_size = 4
    assert_eq!(*captured.lock().unwrap(), Some(4));
}

// ── WPT: transform-streams/cancel.any.js ─────────────────────────────────────
// https://github.com/web-platform-tests/wpt/blob/master/streams/transform-streams/cancel.any.js

// "TransformStream: cancelling the readable side errors the writable side" (spec §6.3.4)
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_readable_errors_writable() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    reader.cancel(Some("consumer done".into())).await.unwrap();

    // Per spec: writable side should now be errored
    assert!(
        writer.write(1u32).await.is_err(),
        "write() must fail after the readable side is cancelled"
    );
}

// "TransformStream: cancelling the readable side with a reason passes that reason to the writable"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_readable_reason_propagates_to_writable() {
    use std::sync::{Arc, Mutex};

    let error_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let error_reason2 = error_reason.clone();

    struct TrackingT {
        error_reason: Arc<Mutex<Option<String>>>,
    }

    impl Transformer<u32, u32> for TrackingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }
    }

    let ts = TransformStream::builder(TrackingT {
        error_reason: error_reason2,
    })
    .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    reader.cancel(Some("the reason".into())).await.unwrap();

    // Writable must be errored — write() should fail
    let write_err = writer.write(1u32).await.unwrap_err();
    // The error message should reflect the cancel reason
    assert!(
        write_err.to_string().contains("the reason"),
        "writable error should contain the cancel reason, got: {write_err}"
    );
    drop(error_reason);
}

// "TransformStream: read() after readable.cancel() returns None"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_readable_subsequent_reads_return_none() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, _writable) = ts.split();
    let (_locked, reader) = readable.get_reader().unwrap();

    reader.cancel(None).await.unwrap();
    // After cancel, reads should return None (stream closed from reader's perspective)
    assert_eq!(reader.read().await.unwrap(), None);
}

// "cancelling the readable side should call transformer.cancel()" (spec §6.3.2)
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_readable_calls_transformer_cancel() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::Transformer;

    let cancel_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cancel_reason2 = cancel_reason.clone();

    struct TrackingT {
        cancel_reason: Arc<Mutex<Option<String>>>,
    }

    impl Transformer<u32, u32> for TrackingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
            *self.cancel_reason.lock().unwrap() = reason;
            Ok(())
        }
    }

    let ts = TransformStream::builder(TrackingT {
        cancel_reason: cancel_reason2,
    })
    .spawn(tokio::spawn);
    let (readable, _writable) = ts.split();
    let (_locked, reader) = readable.get_reader().unwrap();

    reader.cancel(Some("done reading".into())).await.unwrap();

    assert_eq!(
        cancel_reason.lock().unwrap().as_deref(),
        Some("done reading"),
        "Transformer::cancel() must be called with the cancel reason when readable is cancelled"
    );
}

// "aborting the writable side should call transformer.cancel()" (spec §6.3.2)
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_writable_calls_transformer_cancel() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::Transformer;

    let cancel_called: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let cancel_called2 = cancel_called.clone();

    struct TrackingT {
        cancel_called: Arc<Mutex<bool>>,
    }

    impl Transformer<u32, u32> for TrackingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.cancel_called.lock().unwrap() = true;
            Ok(())
        }
    }

    let ts = TransformStream::builder(TrackingT {
        cancel_called: cancel_called2,
    })
    .spawn(tokio::spawn);
    let (_readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();

    writer.abort(Some("aborting".into())).await.unwrap();

    assert!(
        *cancel_called.lock().unwrap(),
        "Transformer::cancel() must be called when the writable side is aborted"
    );
}

// "transformer.cancel() returning Err causes readable.cancel() to reject"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_that_throws_rejects_readable_cancel() {
    use whatwg_streams::Transformer;

    struct RejectingT;

    impl Transformer<u32, u32> for RejectingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err("cancel failed".into())
        }
    }

    let ts = TransformStream::builder(RejectingT).spawn(tokio::spawn);
    let (readable, _writable) = ts.split();
    let (_locked, reader) = readable.get_reader().unwrap();

    let result = reader.cancel(None).await;
    assert!(
        result.is_err(),
        "readable.cancel() must reject when transformer.cancel() throws"
    );
}

// "transformer.cancel() returning Err causes writable.abort() to reject"
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_that_throws_rejects_writable_abort() {
    use whatwg_streams::Transformer;

    struct RejectingT;

    impl Transformer<u32, u32> for RejectingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err("cancel failed".into())
        }
    }

    let ts = TransformStream::builder(RejectingT).spawn(tokio::spawn);
    let (_readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();

    let result = writer.abort(None).await;
    assert!(
        result.is_err(),
        "writable.abort() must reject when transformer.cancel() throws"
    );
}

// ── WPT: transform-streams/backpressure.any.js ───────────────────────────────
// https://github.com/web-platform-tests/wpt/blob/master/streams/transform-streams/backpressure.any.js

// "TransformStream: controller.desiredSize reflects the readable side's HWM"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_desired_size_reflects_readable_hwm() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::CountQueuingStrategy;

    let sizes: Arc<Mutex<Vec<Option<isize>>>> = Arc::new(Mutex::new(Vec::new()));
    let sizes2 = sizes.clone();

    struct SizingT {
        sizes: Arc<Mutex<Vec<Option<isize>>>>,
    }

    impl Transformer<u32, u32> for SizingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // Record desired_size before enqueuing
            self.sizes.lock().unwrap().push(controller.desired_size());
            controller.enqueue(chunk)?;
            // Record desired_size after enqueuing
            self.sizes.lock().unwrap().push(controller.desired_size());
            Ok(())
        }
    }

    // Readable HWM=2: desiredSize starts at 2, decreases as chunks are enqueued
    let ts = TransformStream::builder(SizingT { sizes: sizes2 })
        .readable_strategy(CountQueuingStrategy::new(2))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    writer.write(2u32).await.unwrap();
    writer.close().await.unwrap();

    // Consume all output
    while reader.read().await.unwrap().is_some() {}

    let recorded = sizes.lock().unwrap().clone();
    // HWM=2, two transforms each recording [before_enqueue, after_enqueue].
    // desired_size is computed synchronously from the SpaceSignal pending counter,
    // so values are exact regardless of async readable-task scheduling.
    assert_eq!(
        recorded,
        vec![Some(2), Some(1), Some(1), Some(0)],
        "desired_size must track HWM minus pending enqueues exactly"
    );
}

// "TransformStream: enqueuing more than HWM chunks causes desired_size to go negative"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_desired_size_goes_negative_when_over_hwm() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::CountQueuingStrategy;

    let final_size: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));
    let final_size2 = final_size.clone();

    struct OverfillT {
        final_size: Arc<Mutex<Option<isize>>>,
    }

    impl Transformer<u32, u32> for OverfillT {
        async fn transform(
            &mut self,
            _chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // Enqueue 3 items on a HWM=1 readable — desired_size goes negative.
            // desired_size() is computed synchronously from the SpaceSignal pending
            // counter, so no yield is needed.
            controller.enqueue(1u32)?;
            controller.enqueue(2u32)?;
            controller.enqueue(3u32)?;
            *self.final_size.lock().unwrap() = controller.desired_size();
            controller.terminate()?;
            Ok(())
        }
    }

    let ts = TransformStream::builder(OverfillT {
        final_size: final_size2,
    })
    .readable_strategy(CountQueuingStrategy::new(1)) // HWM=1
    .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(0u32).await.unwrap();

    // Consume all output
    while reader.read().await.unwrap().is_some() {}

    // HWM=1, 3 items enqueued → desired_size = 1 - 3 = -2
    assert_eq!(
        final_size.lock().unwrap().unwrap(),
        -2,
        "desired_size must equal HWM - pending (1 - 3 = -2)"
    );
}

// ── WPT: transform-streams/backpressure.any.js (newly testable) ──────────────

// "backpressure: only one transform() fires per HWM=1 until consumer reads"
// With HWM=1 on the readable and no active reader consuming, writing two chunks
// to the writable should only run transform() once — the second write should
// block on backpressure until the first transformed chunk is consumed.
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_backpressure_blocks_second_write_until_consumer_reads() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::CountQueuingStrategy;

    let transform_count = Arc::new(Mutex::new(0u32));
    let transform_count2 = transform_count.clone();

    struct CountingT {
        count: Arc<Mutex<u32>>,
    }

    impl Transformer<u32, u32> for CountingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.count.lock().unwrap() += 1;
            controller.enqueue(chunk)
        }
    }

    // Readable HWM=1: after one enqueue, desired_size=0 → backpressure
    let ts = TransformStream::builder(CountingT {
        count: transform_count2,
    })
    .readable_strategy(CountQueuingStrategy::new(1))
    .spawn(tokio::spawn);

    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // First write: transform fires, chunk enqueued into readable
    writer.write(1u32).await.unwrap();
    assert_eq!(*transform_count.lock().unwrap(), 1, "first transform must have fired");

    // Queue is now full (HWM=1). Second write should block until we read.
    // Spawn the write so it can proceed concurrently with the read.
    let w = std::sync::Arc::new(writer);
    let wc = w.clone();
    let write2 = tokio::spawn(async move { wc.write(2u32).await });

    // Let the write task start and hit the backpressure wait
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // transform() must NOT have fired a second time yet (backpressure is active)
    assert_eq!(
        *transform_count.lock().unwrap(),
        1,
        "transform() must not fire while readable queue is full (HWM=1)"
    );

    // Consume the first chunk → queue drains → pull() fires → backpressure clears
    assert_eq!(reader.read().await.unwrap(), Some(1));

    // Now the second write can proceed
    write2.await.unwrap().unwrap();
    assert_eq!(*transform_count.lock().unwrap(), 2, "transform() must fire after backpressure clears");

    // Consume second chunk
    assert_eq!(reader.read().await.unwrap(), Some(2));
}

// "backpressure: writes resolve as soon as transform completes when HWM allows"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_no_backpressure_when_reader_keeps_up() {
    use whatwg_streams::CountQueuingStrategy;

    // HWM=10: plenty of room, writes should all complete without blocking
    let ts = TransformStream::builder(DoubleT)
        .readable_strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    for i in 1u32..=5 {
        writer.write(i).await.unwrap();
    }
    writer.close().await.unwrap();

    let mut out = Vec::new();
    while let Some(v) = reader.read().await.unwrap() {
        out.push(v);
    }
    assert_eq!(out, vec![2, 4, 6, 8, 10]);
}

// "transform() should keep being called as long as there is no backpressure"
// A transformer that discards all chunks never enqueues, so pending stays 0,
// has_space stays true, and all writes complete immediately.
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_discard_keeps_all_writes_completing() {
    use whatwg_streams::CountQueuingStrategy;

    struct DiscardT;
    impl Transformer<u32, u32> for DiscardT {
        async fn transform(
            &mut self,
            _chunk: u32,
            _controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Ok(()) // discard — never enqueues
        }
    }

    let ts = TransformStream::builder(DiscardT)
        .readable_strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (_readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();

    // All 4 writes must complete — discard means no backpressure ever builds
    for i in 0u32..4 {
        writer.write(i).await.unwrap();
    }
}

// "writer.closed should resolve after readable is canceled"
// WPT: backpressure.any.js — tests 8 and 10
// Cancelling the readable errors the writable, which should cause writer.closed() to reject.
#[cfg(feature = "send")]
#[tokio::test]
async fn writer_closed_rejects_after_readable_cancel() {
    let ts = TransformStream::builder(DoubleT)
        .readable_strategy(whatwg_streams::CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    reader
        .cancel(Some("consumer cancelled".into()))
        .await
        .unwrap();

    // Readable cancel errors the writable — writer.closed() must reject
    assert!(
        writer.closed().await.is_err(),
        "writer.closed() must reject after the readable side is cancelled"
    );
}

// "cancelling the readable should cause a pending write to resolve"
// WPT: backpressure.any.js test 11
// A write blocked by backpressure (HWM=1 queue full) must complete (not hang) after
// readable.cancel() fires pull_fired().  The write may succeed or error depending on
// whether the transform task picks it before the cancel message, but it must not hang.
//
// We use a no-enqueue transformer so the transform itself never blocks; the test
// verifies that cancel clears the backpressure gate and writer.closed() rejects.
#[cfg(feature = "send")]
#[tokio::test]
async fn readable_cancel_clears_backpressure_and_closes_writer() {
    use whatwg_streams::CountQueuingStrategy;

    // EnqueueOnceT fills the queue on the first write (creating backpressure),
    // then discards subsequent writes so transform() on a cancelled readable never
    // tries to enqueue again.
    struct EnqueueOnceT {
        first: bool,
    }

    impl Transformer<u32, u32> for EnqueueOnceT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            if self.first {
                self.first = false;
                // enqueue — fills the HWM=1 queue, triggering backpressure
                controller.enqueue(chunk)?;
            }
            // subsequent calls: discard so we don't enqueue on the cancelled readable
            Ok(())
        }
    }

    let ts = TransformStream::builder(EnqueueOnceT { first: true })
        .readable_strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);

    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // First write fills the readable queue (HWM=1) → has_space becomes false
    writer.write(1u32).await.unwrap();

    // Second write is now blocked by backpressure; spawn it concurrently
    let w = std::sync::Arc::new(writer);
    let wc = w.clone();
    let write2 = tokio::spawn(async move { wc.write(2u32).await });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Cancel the readable — pull_fired() clears the backpressure gate,
    // unblocking write2 (which will not hang).
    reader.cancel(None).await.unwrap();

    // write2 must complete (resolve or error) — it must not hang
    let _ = write2.await.unwrap();

    // writer.closed() must reject (writable errored by cancel)
    assert!(
        w.closed().await.is_err(),
        "writer.closed() must reject after readable cancel"
    );
}

// "calling pull() before the first write() with backpressure should work"
// WPT: transform-streams/backpressure.any.js test 5
//
// reader.read() is issued before any writer.write().  The read creates a
// pending_read in the readable task; when transform() later enqueues, the
// chunk is delivered directly to the pending read without touching the queue.
// This path is independent of HWM (the WPT uses HWM=0 but our .max(1)
// substitution is invisible here because the pending-read path bypasses the
// queue entirely).
#[cfg(feature = "send")]
#[tokio::test]
async fn read_before_write_delivers_chunk() {
    let ts = TransformStream::builder(DoubleT)
        .readable_strategy(whatwg_streams::CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // Spawn the read first — it will pend until a chunk arrives
    let read_fut = tokio::spawn(async move { reader.read().await });

    // Yield so the read request reaches the readable task before the write
    tokio::task::yield_now().await;

    writer.write(1u32).await.unwrap();

    // The pending read must have been satisfied with the transformed chunk
    assert_eq!(
        read_fut.await.unwrap().unwrap(),
        Some(2),
        "pending read must receive the transformed chunk delivered before the queue"
    );
}
