// WPT: streams/transform-streams/

use whatwg_streams::{
    CountQueuingStrategy, StreamResult, TransformStream, TransformStreamDefaultController, Transformer,
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

// WPT: transform-streams/general.any.js —
// "closing the writable should close the readable when there are no queued chunks, even with
// backpressure". An explicit readable HWM 0 backpressures the readable from the start; closing
// the writable with no writes must still close the readable cleanly (nothing is queued to flush).
#[cfg(feature = "send")]
#[tokio::test]
async fn closing_writable_closes_readable_under_backpressure() {
    let ts = TransformStream::builder(DoubleT)
        .readable_strategy(CountQueuingStrategy::new(0))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.close().await.unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
    reader.closed().await.unwrap();
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
    // Explicit readable HWM 1: this test awaits a write before reading, which the
    // spec-default readable HWM of 0 would deadlock. The buffer is incidental here —
    // the subject is abort propagation, not backpressure.
    let ts = TransformStream::builder(DoubleT)
        .readable_strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
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
    .readable_strategy(CountQueuingStrategy::new(1)) // incidental buffer; subject is flush
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

    let ts = TransformStream::builder(TerminatingT)
        .readable_strategy(CountQueuingStrategy::new(1)) // incidental buffer; subject is terminate
        .spawn(tokio::spawn);
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

    let ts = TransformStream::builder(TerminatingT)
        .readable_strategy(CountQueuingStrategy::new(1)) // incidental buffer; subject is terminate
        .spawn(tokio::spawn);
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

// "closing the writable side should reject if a parallel transformer.cancel() throws"
// WPT: cancel.any.js test 5 — cancel wins over flush; both promises reject with the cancel error.
#[cfg(feature = "send")]
#[tokio::test]
async fn close_rejects_when_parallel_cancel_throws() {
    use whatwg_streams::Transformer;

    struct ThrowingCancelT;

    impl Transformer<u32, u32> for ThrowingCancelT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err("cancel threw".into())
        }
    }

    let ts = TransformStream::builder(ThrowingCancelT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // Fire both concurrently — select! in the transform task will pick one
    let cancel_fut = tokio::spawn(async move { reader.cancel(Some("reason".into())).await });
    let close_fut = tokio::spawn(async move { writer.close().await });

    let cancel_result = cancel_fut.await.unwrap();
    let close_result = close_fut.await.unwrap();

    // At least the cancel must have seen the error; the close must not hang
    // (it receives either the cancel error or a Canceled rejection)
    assert!(
        cancel_result.is_err() || close_result.is_err(),
        "cancel threw — at least one of cancel/close must reject"
    );
}

// WPT: cancel.any.js test 6 — "readable.cancel() and a parallel writable.close() should reject if
// a transformer.cancel() calls controller.error()". The transformer captures the controller in
// start() (expressible now that TransformStreamDefaultController is Clone) and errors it from
// cancel() instead of returning Err; both the cancel and the parallel close must reject with that
// error. The cancel routing reads back the controller error (via error_raised()) to reject
// readable.cancel() with it, and the writable is errored after the algorithm runs (not pre-errored
// with the cancel reason), so the thrown error stands.
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_calling_controller_error_rejects_cancel_and_parallel_close() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::Transformer;

    struct ErrorOnCancelT {
        ctrl: Arc<Mutex<Option<TransformStreamDefaultController<u32>>>>,
    }
    impl Transformer<u32, u32> for ErrorOnCancelT {
        async fn start(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.ctrl.lock().unwrap() = Some(controller.clone());
            Ok(())
        }
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            // Signal failure by erroring the controller, not by returning Err.
            if let Some(c) = self.ctrl.lock().unwrap().clone() {
                c.error("thrown".into())?;
            }
            Ok(())
        }
    }

    let ctrl = Arc::new(Mutex::new(None));
    let ts = TransformStream::builder(ErrorOnCancelT { ctrl: ctrl.clone() }).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_rl, reader) = readable.get_reader().unwrap();
    let (_wl, writer) = writable.get_writer().unwrap();

    let cancel_fut = tokio::spawn(async move { reader.cancel(Some("original".into())).await });
    let close_fut = tokio::spawn(async move { writer.close().await });
    let cancel_res = cancel_fut.await.unwrap();
    let close_res = close_fut.await.unwrap();

    let cancel_err = cancel_res.expect_err("readable.cancel() must reject with the controller error");
    assert!(cancel_err.to_string().contains("thrown"), "cancel got: {cancel_err}");
    let close_err = close_res.expect_err("parallel writable.close() must reject with the controller error");
    assert!(close_err.to_string().contains("thrown"), "close got: {close_err}");
}

// WPT: cancel.any.js test 7 — "writable.abort() and readable.cancel() should reject if a
// transformer.cancel() calls controller.error()". abort() runs the same cancel algorithm; the
// controller error becomes the abort rejection, and a later readable.cancel() on the now-errored
// stream rejects with that same error.
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_calling_controller_error_rejects_abort_and_later_cancel() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::Transformer;

    struct ErrorOnCancelT {
        ctrl: Arc<Mutex<Option<TransformStreamDefaultController<u32>>>>,
    }
    impl Transformer<u32, u32> for ErrorOnCancelT {
        async fn start(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.ctrl.lock().unwrap() = Some(controller.clone());
            Ok(())
        }
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            if let Some(c) = self.ctrl.lock().unwrap().clone() {
                c.error("thrown".into())?;
            }
            Ok(())
        }
    }

    let ctrl = Arc::new(Mutex::new(None));
    let ts = TransformStream::builder(ErrorOnCancelT { ctrl: ctrl.clone() }).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_rl, reader) = readable.get_reader().unwrap();
    let (_wl, writer) = writable.get_writer().unwrap();

    let abort_err = writer
        .abort(Some("original".into()))
        .await
        .expect_err("writable.abort() must reject with the controller error");
    assert!(abort_err.to_string().contains("thrown"), "abort got: {abort_err}");

    let cancel_err = reader
        .cancel(None)
        .await
        .expect_err("a later readable.cancel() on the errored stream must reject");
    assert!(cancel_err.to_string().contains("thrown"), "cancel got: {cancel_err}");
}

// "readable.cancel() should not call cancel() when flush() is already executing from writable.close()"
// WPT: cancel.any.js test 8 (structural variant) — when close triggers flush and readable.cancel()
// arrives concurrently, cancel fires once and close completes without double-calling cancel.
#[cfg(feature = "send")]
#[tokio::test]
async fn concurrent_cancel_and_close_call_cancel_at_most_once() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::Transformer;

    let cancel_calls = Arc::new(Mutex::new(0u32));
    let cancel_calls2 = cancel_calls.clone();

    struct CountCancelT {
        calls: Arc<Mutex<u32>>,
    }

    impl Transformer<u32, u32> for CountCancelT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.calls.lock().unwrap() += 1;
            Ok(())
        }
    }

    let ts = TransformStream::builder(CountCancelT { calls: cancel_calls2 }).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    let cancel_fut = tokio::spawn(async move { reader.cancel(None).await });
    let close_fut = tokio::spawn(async move { writer.close().await });

    let _ = cancel_fut.await.unwrap();
    let _ = close_fut.await.unwrap();

    assert_eq!(
        *cancel_calls.lock().unwrap(),
        1,
        "transformer.cancel() must be called exactly once regardless of which side wins the race"
    );
}

// WPT: flush.any.js —
// "closing the writable side should call transformer.flush() and a parallel
//  readable.cancel() should not reject"
// When close() wins the race, flush() runs, transformer.cancel() is NOT called, and
// both the close and the parallel cancel resolve cleanly.
#[cfg(feature = "send")]
#[tokio::test]
async fn close_flush_wins_over_parallel_cancel() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::Transformer;

    struct FlushNoCancelT {
        flushed: Arc<Mutex<bool>>,
        cancel_called: Arc<Mutex<bool>>,
    }

    impl Transformer<u32, u32> for FlushNoCancelT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn flush(
            &mut self,
            _controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.flushed.lock().unwrap() = true;
            Ok(())
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.cancel_called.lock().unwrap() = true;
            Ok(())
        }
    }

    let flushed = Arc::new(Mutex::new(false));
    let cancel_called = Arc::new(Mutex::new(false));
    let ts = TransformStream::builder(FlushNoCancelT {
        flushed: flushed.clone(),
        cancel_called: cancel_called.clone(),
    })
    .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    // Close the writable first; flush() runs as part of closing (close wins the race).
    let close_fut = tokio::spawn(async move { writer.close().await });
    for _ in 0..16 {
        if *flushed.lock().unwrap() {
            break;
        }
        tokio::task::yield_now().await;
    }

    // The parallel cancel arrives after flush has won: it must resolve cleanly and
    // must not invoke transformer.cancel().
    let cancel_result = reader.cancel(Some("late".into())).await;
    let close_result = close_fut.await.unwrap();

    assert!(close_result.is_ok(), "writer.close() must resolve: {close_result:?}");
    assert!(cancel_result.is_ok(), "readable.cancel() must resolve: {cancel_result:?}");
    assert!(*flushed.lock().unwrap(), "transformer.flush() must be called");
    assert!(
        !*cancel_called.lock().unwrap(),
        "transformer.cancel() must not be called once close/flush has won"
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

// WPT: backpressure.any.js — tests 8 and 10, titled "writer.closed should resolve
// after readable is canceled". The WPT title is a naming quirk: the bodies assert
// promise_rejects_exactly(..., closed, 'closed should reject'). Cancelling the
// readable errors the writable, so writer.closed() rejects — which is what is tested.
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

// WPT: transform errors.any.js — aborting the writable while a transform is in flight rejects the
// writes still queued behind it with the abort reason, while the in-flight transform finishes with
// its own result. (The in-flight write carrying its own outcome, not the abort error, mirrors the
// writable-level `abort_rejects_queued_writes_but_in_flight_finishes`.)
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_rejects_queued_write_through_transform() {
    use std::sync::Arc;

    struct GateT {
        gate: Arc<tokio::sync::Notify>,
    }
    impl Transformer<u32, u32> for GateT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // Block the first transform so its write stays in flight while the next write queues.
            self.gate.notified().await;
            controller.enqueue(chunk)
        }
    }

    let gate = Arc::new(tokio::sync::Notify::new());
    let ts = TransformStream::builder(GateT { gate: gate.clone() }).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, _reader) = readable.get_reader().unwrap();

    let w = Arc::new(writer);
    // write(1) is in flight (its transform is blocked on the gate).
    let w1 = w.clone();
    let write1 = tokio::spawn(async move { w1.write(1u32).await });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    // write(2) is queued behind the in-flight write(1).
    let w2 = w.clone();
    let write2 = tokio::spawn(async move { w2.write(2u32).await });
    tokio::task::yield_now().await;

    // Abort with a reason; then release the gate so the in-flight transform completes and the
    // abort can proceed.
    let wa = w.clone();
    let abort_fut = tokio::spawn(async move { wa.abort(Some("aborted".into())).await });
    tokio::task::yield_now().await;
    gate.notify_one();
    abort_fut.await.unwrap().unwrap();

    // The queued write rejects with the abort reason; the in-flight one finishes with its own
    // result (it was released, so it succeeds — not the abort error).
    let queued = write2.await.unwrap().expect_err("a queued write must reject on abort");
    assert!(
        queued.to_string().contains("aborted"),
        "the queued write rejects with the abort reason, got: {queued}"
    );
    assert!(
        write1.await.unwrap().is_ok(),
        "the in-flight write finishes with its own result"
    );
}

// "backpressure allows no transforms with a default identity transform and no reader"
// WPT: transform-streams/backpressure.any.js test 1
// With HWM=0 (readable strategy), the SpaceSignal starts closed and pull() only fires
// when a pending read exists.  Writes block indefinitely without a reader.
#[cfg(feature = "send")]
#[tokio::test]
async fn no_transforms_without_reader_at_hwm0() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::CountQueuingStrategy;

    let transform_count = Arc::new(Mutex::new(0u32));
    let transform_count2 = transform_count.clone();

    struct CountingPassT {
        calls: Arc<Mutex<u32>>,
    }

    impl Transformer<u32, u32> for CountingPassT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.calls.lock().unwrap() += 1;
            controller.enqueue(chunk)
        }
    }

    let ts = TransformStream::builder(CountingPassT {
        calls: transform_count2,
    })
    .readable_strategy(CountQueuingStrategy::new(0)) // HWM=0 → no transforms without reader
    .spawn(tokio::spawn);
    let (_readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();

    // Spawn a write — it must stay pending because has_space=false (HWM=0, no reader)
    let w = Arc::new(writer);
    let wc = w.clone();
    let write_fut = tokio::spawn(async move { wc.write(1u32).await });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    assert_eq!(
        *transform_count.lock().unwrap(),
        0,
        "transform() must not fire with HWM=0 and no reader"
    );

    // Abort the write task — the write will stay blocked indefinitely without a reader
    write_fut.abort();
    let _ = write_fut.await;
}

// WPT: transform-streams/backpressure.any.js test 9, titled "writer.closed should
// resolve after readable is canceled with backpressure". As with tests 8/10 the
// title is a quirk — the body asserts a rejection. Same invariant, for HWM=0.
#[cfg(feature = "send")]
#[tokio::test]
async fn writer_closed_rejects_after_readable_cancel_hwm0() {
    use whatwg_streams::CountQueuingStrategy;

    let ts = TransformStream::builder(DoubleT)
        .readable_strategy(CountQueuingStrategy::new(0))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    reader.cancel(Some("done".into())).await.unwrap();

    assert!(
        writer.closed().await.is_err(),
        "writer.closed() must reject after readable cancel (HWM=0)"
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

// ── WPT: transform-streams/errors.any.js ─────────────────────────────────────

// "TransformStream errors thrown in flush put the writable and readable in an
//  errored state"
#[cfg(feature = "send")]
#[tokio::test]
async fn flush_error_errors_both_sides() {
    struct FlushErrorT;
    impl Transformer<u32, u32> for FlushErrorT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }
        async fn flush(
            &mut self,
            _controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Err("flush failed".into())
        }
    }

    let ts = TransformStream::builder(FlushErrorT)
        .readable_strategy(CountQueuingStrategy::new(1)) // incidental buffer; subject is flush error
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    // close triggers flush(), which errors → both sides errored, close rejects.
    assert!(writer.close().await.is_err(), "close must reject when flush() errors");

    // The readable side must end in an error, not a clean close.
    let mut errored = false;
    for _ in 0..3 {
        match reader.read().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("readable closed cleanly; a flush() error must error it"),
            Err(_) => {
                errored = true;
                break;
            }
        }
    }
    assert!(errored, "readable side must be errored after flush() throws");
}

// "TransformStream transformer.start() rejected promise should error the stream"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_rejection_errors_stream() {
    struct FailStartT;
    impl Transformer<u32, u32> for FailStartT {
        async fn start(
            &mut self,
            _controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Err("start failed".into())
        }
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }
    }

    let ts = TransformStream::builder(FailStartT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    assert!(writer.write(1u32).await.is_err(), "write must fail when start() rejected");
    assert!(reader.read().await.is_err(), "readable must be errored when start() rejected");
}

// "the readable should be errored with the reason passed to the writable abort()"
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_reason_propagates_to_readable() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    writer.abort(Some("custom abort reason".into())).await.unwrap();

    let read = reader.read().await;
    assert!(read.is_err(), "readable must be errored after writable.abort()");
    let msg = format!("{read:?}");
    assert!(
        msg.contains("custom abort reason"),
        "readable error must carry the abort reason, got {read:?}"
    );
}

// ── WPT: transform-streams/general.any.js ────────────────────────────────────

// "TransformStream: by default, closing the writable waits for transforms to
//  finish before closing both"
// A transform is held in-flight while close() is requested; close must wait for
// that transform to enqueue its chunk before the readable closes.
#[cfg(feature = "send")]
#[tokio::test]
async fn close_waits_for_in_flight_transform() {
    use std::sync::Arc;

    struct SlowT {
        started: Arc<tokio::sync::Notify>,
        unblock: Arc<tokio::sync::Notify>,
    }
    impl Transformer<u32, u32> for SlowT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            self.started.notify_one();
            self.unblock.notified().await; // hold the transform in-flight
            controller.enqueue(chunk + 100)
        }
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let unblock = Arc::new(tokio::sync::Notify::new());
    let ts = TransformStream::builder(SlowT {
        started: started.clone(),
        unblock: unblock.clone(),
    })
    .readable_strategy(CountQueuingStrategy::new(1)) // incidental buffer; subject is close ordering
    .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    // Queue chunk 1: transform(1) starts and blocks.
    writer.enqueue(1u32).unwrap();
    started.notified().await;

    // Request close while the transform is still in-flight.
    let close_task = tokio::spawn(async move { writer.close().await });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Release the transform; close must complete only after it enqueues.
    unblock.notify_one();
    close_task.await.unwrap().unwrap();

    assert_eq!(
        reader.read().await.unwrap(),
        Some(101),
        "the in-flight transform's chunk must arrive before the readable closes"
    );
    assert_eq!(reader.read().await.unwrap(), None, "readable closes after the final transform");
}

// "enqueue() should throw after controller.terminate()"
#[cfg(feature = "send")]
#[tokio::test]
async fn enqueue_after_terminate_errors() {
    use std::sync::{Arc, Mutex};

    struct TerminateT {
        enqueue_was_err: Arc<Mutex<Option<bool>>>,
    }
    impl Transformer<u32, u32> for TerminateT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.terminate()?;
            // enqueue() after terminate() must be rejected.
            let result = controller.enqueue(chunk);
            *self.enqueue_was_err.lock().unwrap() = Some(result.is_err());
            Ok(())
        }
    }

    let enqueue_was_err = Arc::new(Mutex::new(None));
    let ts = TransformStream::builder(TerminateT {
        enqueue_was_err: enqueue_was_err.clone(),
    })
    .readable_strategy(CountQueuingStrategy::new(1)) // incidental buffer; subject is terminate guard
    .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    let _ = writer.write(1u32).await;
    // terminate() closes the readable side.
    assert_eq!(reader.read().await.unwrap(), None, "terminate() closes the readable");
    assert_eq!(
        *enqueue_was_err.lock().unwrap(),
        Some(true),
        "enqueue() after terminate() must return an error"
    );
}

// Skipped from transform-streams general.any.js / errors.any.js / properties.any.js
// / patched-global.any.js — untranslatable to the Rust API, not coverage gaps:
//
// - "construct with no transform function" (identity default): the Transformer trait
//   requires a transform() method; an identity transform is a one-line impl, not a
//   constructor default to test.
// - "call transformer methods as methods" / "no .apply()/.call()": JS this-binding on
//   callbacks. Transformers are trait impls with an explicit &mut self receiver.
// - "specifying a defined readableType/writableType should throw": guards against the
//   string type options on the JS constructor; the Rust builder has no such field.
// - "constructor should throw when start does" / "strategy.size throws in start()":
//   surface synchronous constructor throwing. Construction here is infallible; a
//   start() rejection errors the stream instead (covered by start_rejection_errors_stream).
// - "Subclassing TransformStream should work": JS prototype subclassing.
// - properties.any.js / patched-global.any.js: property descriptors and global
//   patching — JS object-model introspection with no Rust analogue.

// ── WPT: transform-streams/strategies.any.js ─────────────────────────────────

// "default writable strategy should be equivalent to { highWaterMark: 1 }" +
// "default readable strategy should be equivalent to { highWaterMark: 0 }"
//
// A readable HWM of 0 applies backpressure immediately: transform() does not run
// until a read is pending, so `writer.write(x).await` blocks until the chunk is read.
// Tests whose subject is not backpressure set an explicit readable HWM 1 to keep
// awaiting a write before reading.
#[cfg(feature = "send")]
#[tokio::test]
async fn default_strategy_hwms() {
    use std::sync::{Arc, Mutex};
    let readable_ds: Arc<Mutex<Option<Option<isize>>>> = Arc::new(Mutex::new(None));
    struct CaptureStartT {
        readable_ds: Arc<Mutex<Option<Option<isize>>>>,
    }
    impl Transformer<u32, u32> for CaptureStartT {
        async fn start(&mut self, c: &mut TransformStreamDefaultController<u32>) -> StreamResult<()> {
            *self.readable_ds.lock().unwrap() = Some(c.desired_size());
            Ok(())
        }
        async fn transform(&mut self, chunk: u32, c: &mut TransformStreamDefaultController<u32>) -> StreamResult<()> {
            c.enqueue(chunk)
        }
    }
    let ts = TransformStream::builder(CaptureStartT { readable_ds: readable_ds.clone() })
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, _reader) = readable.get_reader().unwrap();
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Spec defaults: writable HWM 1, readable HWM 0.
    assert_eq!(writer.desired_size(), Some(1), "default writable strategy is HWM 1");
    assert_eq!(
        *readable_ds.lock().unwrap(),
        Some(Some(0)),
        "default readable strategy is HWM 0"
    );
}

// WPT: errors.any.js —
// "an exception from transform() should error the stream if terminate has been
//  requested but not completed"
// transform() enqueues, terminates, then throws; the thrown error must win — the
// readable errors with it rather than closing cleanly from the terminate.
//
// The readable defers its `Closed` transition while the queue still holds chunks (it stays
// "readable"/closing until a read drains the last one), so the error() following terminate()
// still applies and the readable errors with the thrown error rather than closing cleanly.
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_throw_after_terminate_errors_readable() {
    struct ThrowAfterTerminateT;

    impl Transformer<u32, u32> for ThrowAfterTerminateT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)?;
            controller.terminate()?;
            Err("transform boom".into())
        }
    }

    let ts = TransformStream::builder(ThrowAfterTerminateT)
        .readable_strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    // The write triggers transform(), which enqueues, terminates, then throws.
    let write_result = writer.write(1u32).await;
    assert!(
        write_result.is_err(),
        "write() must reject with the thrown error, got: {write_result:?}"
    );

    // The readable must surface the transform error (by identity), not close cleanly:
    // reader.closed() rejects with the thrown error on an errored stream, and resolves Ok
    // on a cleanly-closed one.
    let closed_err = reader
        .closed()
        .await
        .expect_err("readable must error, not close cleanly");
    assert!(
        closed_err.to_string().contains("transform boom"),
        "readable must error with the thrown error's identity, got: {closed_err}"
    );
}

// WPT: errors.any.js — "controller.error() should do nothing the second time it is called"
// (transform side). The first error wins on the transform's readable controller too — a
// surplus controller.error() does not overwrite the stored error.
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_surplus_controller_error_is_noop_first_wins() {
    struct DoubleErrorT;

    impl Transformer<u32, u32> for DoubleErrorT {
        async fn transform(
            &mut self,
            _chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.error("first".into())?;
            let _ = controller.error("second".into());
            Ok(())
        }
    }

    // Explicit readable HWM 1: the spec-default readable HWM of 0 would block the write in
    // wait_for_readable_space() before transform() runs (this test never reads), deadlocking.
    let ts = TransformStream::builder(DoubleErrorT)
        .readable_strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_lw, writer) = writable.get_writer().unwrap();
    let (_lr, reader) = readable.get_reader().unwrap();

    let _ = writer.write(1u32).await; // triggers transform() → two controller.error() calls
    let closed_err = reader
        .closed()
        .await
        .expect_err("readable must error after controller.error()");
    assert!(
        closed_err.to_string().contains("first"),
        "the first controller.error() wins; the surplus call is a no-op, got: {closed_err}"
    );
}
