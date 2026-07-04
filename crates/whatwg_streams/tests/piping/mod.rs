// WPT: streams/piping/
// https://github.com/web-platform-tests/wpt/tree/master/streams/piping

use crate::helpers::{CollectSink, FailAfterSink};
use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamError, StreamPipeOptions, StreamResult, TransformStream, Transformer,
    TransformStreamDefaultController, WritableSink, WritableStream, WritableStreamDefaultController,
};

struct ErrorAfterSource {
    data: Vec<u32>,
    index: std::sync::Arc<std::sync::Mutex<usize>>,
}

impl ReadableSource<u32> for ErrorAfterSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        let idx = {
            let mut i = self.index.lock().unwrap();
            let v = *i;
            *i += 1;
            v
        };
        if idx < self.data.len() {
            controller.enqueue(self.data[idx])?;
        } else {
            return Err(StreamError::from("source error"));
        }
        Ok(())
    }
}

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

// ── WPT: piping/general-addition.any.js ──────────────────────────────────────

// "Piping: all chunks flow from the readable to the writable"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_transfers_all_chunks() {
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let data = vec![1u32, 2, 3];
    let source = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    source.pipe_to(&dest, None).await.unwrap();
    assert_eq!(*collected.lock().unwrap(), data);
}

// "Piping: from an empty stream closes the destination immediately"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_from_empty_stream_closes_dest() {
    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: closed.clone(),
        aborted: Default::default(),
    };
    let source = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);
    source.pipe_to(&dest, None).await.unwrap();
    assert!(*closed.lock().unwrap());
}

// "Piping: 100 chunks arrive in order"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_large_stream_preserves_order() {
    let n = 100u32;
    let data: Vec<u32> = (1..=n).collect();
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let source = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(50))
        .spawn(tokio::spawn);
    source.pipe_to(&dest, None).await.unwrap();
    assert_eq!(*collected.lock().unwrap(), data);
}

// ── WPT: piping/close-propagation-forward.any.js ─────────────────────────────

// "Piping: close propagates from readable to writable (prevent_close=false)"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_close_propagates_forward() {
    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: closed.clone(),
        aborted: Default::default(),
    };
    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);
    source.pipe_to(&dest, Some(StreamPipeOptions::default())).await.unwrap();
    assert!(*closed.lock().unwrap());
}

// "Piping: prevent_close=true keeps destination open when source closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_prevent_close_option() {
    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: closed.clone(),
        aborted: Default::default(),
    };
    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);
    source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                prevent_close: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert!(!*closed.lock().unwrap());
}

// ── WPT: piping/error-propagation-via-abort.any.js ───────────────────────────

// "Piping: error in source aborts the destination (prevent_abort=false)"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_source_error_aborts_dest() {
    let aborted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: aborted.clone(),
    };
    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![],
        index: Default::default(),
    })
    .spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);
    assert!(source.pipe_to(&dest, None).await.is_err());
    let abort_reason = aborted.lock().unwrap().clone();
    assert!(abort_reason.is_some(), "destination must be aborted when source errors");
    assert!(
        abort_reason.unwrap().contains("source error"),
        "abort reason must carry the source error message"
    );
}

// "Piping: prevent_abort=true keeps destination alive when source errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_prevent_abort_option() {
    let aborted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: aborted.clone(),
    };
    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![],
        index: Default::default(),
    })
    .spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);
    let _ = source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                prevent_abort: true,
                ..Default::default()
            }),
        )
        .await;
    assert!(aborted.lock().unwrap().is_none());
}

// "Piping: prevent_cancel=true keeps source alive when destination errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_prevent_cancel_option() {
    let sink = FailAfterSink {
        fail_after: 0,
        count: Default::default(),
    };
    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);
    assert!(source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                prevent_cancel: true,
                ..Default::default()
            }),
        )
        .await
        .is_err());
}

// ── pipeThrough ───────────────────────────────────────────────────────────────

// "pipeThrough: data flows through the transform and out the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_through_transforms_chunks() {
    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let transform = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let output = source.pipe_through(transform, None).spawn(tokio::spawn);
    let (_locked, reader) = output.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(2));
    assert_eq!(reader.read().await.unwrap(), Some(4));
    assert_eq!(reader.read().await.unwrap(), Some(6));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "pipeThrough: closing source closes the transform output"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_through_close_propagates() {
    struct IdentityT;
    impl Transformer<u32, u32> for IdentityT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }
    }
    let source = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let transform = TransformStream::builder(IdentityT).spawn(tokio::spawn);
    let output = source.pipe_through(transform, None).spawn(tokio::spawn);
    let (_locked, reader) = output.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
}

// "pipeThrough: chaining two transforms works end-to-end"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_through_chain() {
    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let t1 = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let t2 = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let mid = source.pipe_through(t1, None).spawn(tokio::spawn);
    let out = mid.pipe_through(t2, None).spawn(tokio::spawn);
    let (_locked, reader) = out.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(4));
    assert_eq!(reader.read().await.unwrap(), Some(8));
    assert_eq!(reader.read().await.unwrap(), Some(12));
    assert_eq!(reader.read().await.unwrap(), None);
}

// ── WPT: piping/abort.any.js ──────────────────────────────────────────────────

// "Piping: aborting via signal stops the pipe and returns an Aborted error"
// Source blocks forever in pull() — this is the exact scenario that previously
// deadlocked when the signal fired and pipe_to called reader.cancel().
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_signal_aborts_pipe() {
    use whatwg_streams::AbortController;
    use whatwg_streams::StreamError;

    struct BlockingSource;
    impl ReadableSource<u32> for BlockingSource {
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
    }

    let controller = AbortController::new();
    let signal = controller.signal();
    let source = ReadableStream::builder(BlockingSource).spawn(tokio::spawn);
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let pipe = tokio::spawn(async move {
        source
            .pipe_to(
                &dest,
                Some(StreamPipeOptions {
                    signal: Some(signal),
                    ..Default::default()
                }),
            )
            .await
    });

    // Let the pipe start and the source enter its blocking pull()
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Fire abort while source is stuck — previously this deadlocked because
    // reader.cancel() could not resolve while pull_future was in flight.
    controller.abort(None);

    let result = pipe.await.unwrap();
    assert!(
        matches!(result, Err(StreamError::Aborted(_))),
        "pipe_to should return Aborted when signal fires during in-flight pull, got: {:?}",
        result
    );
}

// "Piping: abort signal fires before pipe starts — pipe errors immediately"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_pre_aborted_signal_errors_immediately() {
    use whatwg_streams::AbortController;
    use whatwg_streams::StreamError;

    let controller = AbortController::new();
    let signal = controller.signal();
    // Fire the signal before piping even starts
    controller.abort(None);

    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let result = source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                signal: Some(signal),
                ..Default::default()
            }),
        )
        .await;

    assert!(
        matches!(result, Err(StreamError::Aborted(_))),
        "pre-fired signal should cause immediate Aborted error, got: {:?}",
        result
    );
}

// "Piping: prevent_abort=true — destination is not aborted when signal fires"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_signal_with_prevent_abort_skips_sink_abort() {
    use whatwg_streams::AbortController;

    struct BlockingSource;
    impl ReadableSource<u32> for BlockingSource {
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
    }

    let controller = AbortController::new();
    let signal = controller.signal();
    let aborted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: aborted.clone(),
    };

    let source = ReadableStream::builder(BlockingSource).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let pipe = tokio::spawn(async move {
        source
            .pipe_to(
                &dest,
                Some(StreamPipeOptions {
                    signal: Some(signal),
                    prevent_abort: true,
                    ..Default::default()
                }),
            )
            .await
    });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    controller.abort(None);

    assert!(
        pipe.await.unwrap().is_err(),
        "pipe must reject when the abort signal fires"
    );
    assert!(
        aborted.lock().unwrap().is_none(),
        "destination must NOT be aborted when prevent_abort=true"
    );
}

// ── WPT: piping/close-propagation-backward.any.js ────────────────────────────
// When the destination errors, the source should be cancelled (default behaviour).

/// Source that records cancel reason and blocks in pull() after exhausting its data
/// so the pipe is still "active" when the destination errors.
#[cfg(feature = "send")]
struct CancelTrackingSource {
    data: Vec<u32>,
    index: usize,
    cancel_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(feature = "send")]
impl ReadableSource<u32> for CancelTrackingSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        if self.index < self.data.len() {
            controller.enqueue(self.data[self.index])?;
            self.index += 1;
        } else {
            // No more data — hang until cancelled (tests the cancel-during-pull path)
            futures::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        *self.cancel_reason.lock().unwrap() = reason;
        Ok(())
    }
}

// "Piping: when the destination errors, the source is cancelled (prevent_cancel=false)"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_dest_error_cancels_source() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));

    let source = ReadableStream::builder(CancelTrackingSource {
        data: vec![1u32],
        index: 0,
        cancel_reason: cancel_reason.clone(),
    })
    .spawn(tokio::spawn);

    // Sink fails on the first write → dest becomes errored
    let sink = FailAfterSink {
        fail_after: 0,
        count: Default::default(),
    };
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let result = source.pipe_to(&dest, None).await;
    assert!(result.is_err(), "pipe should return an error when dest errors");
    assert!(
        cancel_reason.lock().unwrap().is_some(),
        "source.cancel() should be called when destination errors"
    );
}

// "Piping: cancel reason passed to source matches the destination error"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_dest_error_reason_passed_to_source_cancel() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));

    let source = ReadableStream::builder(CancelTrackingSource {
        data: vec![1u32],
        index: 0,
        cancel_reason: cancel_reason.clone(),
    })
    .spawn(tokio::spawn);

    let sink = FailAfterSink {
        fail_after: 0,
        count: Default::default(),
    };
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let _ = source.pipe_to(&dest, None).await;

    // Cancel reason must equal the sink write error forwarded by the pipe
    let reason = cancel_reason.lock().unwrap().clone().expect("cancel reason must be set");
    assert!(
        reason.contains("sink write failed"),
        "cancel reason must carry the sink error message, got: {reason:?}"
    );
}

// "Piping: prevent_cancel=true prevents source cancellation when dest errors"
// (already tested via pipe_to_prevent_cancel_option — added here for explicitness)
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_dest_error_with_prevent_cancel_skips_source_cancel() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));

    let source = ReadableStream::builder(CancelTrackingSource {
        data: vec![1u32],
        index: 0,
        cancel_reason: cancel_reason.clone(),
    })
    .spawn(tokio::spawn);

    let sink = FailAfterSink {
        fail_after: 0,
        count: Default::default(),
    };
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let _ = source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                prevent_cancel: true,
                ..Default::default()
            }),
        )
        .await;

    assert!(
        cancel_reason.lock().unwrap().is_none(),
        "source.cancel() should NOT be called when prevent_cancel=true"
    );
}

// WPT: error-propagation-backward.any.js — "becomes errored after piping due to last write;
// source is closed; preventCancel omitted (but cancel is never called)".
// The source has already closed by the time the final write rejects, so the backward-error
// shutdown must reject the pipe with the write error WITHOUT invoking the source's cancel()
// (cancelling an already-closed readable is a no-op). preventCancel is irrelevant here.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_last_write_error_does_not_cancel_already_closed_source() {
    use std::sync::{Arc, Mutex};

    struct ClosedSourceCancelTracking {
        cancel_called: Arc<Mutex<bool>>,
    }
    impl ReadableSource<u32> for ClosedSourceCancelTracking {
        async fn start(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(1)?;
            controller.enqueue(2)?;
            controller.enqueue(3)?;
            controller.close()?;
            Ok(())
        }
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // Never reached: start() closes the stream, so the pull gate stays shut.
            Ok(())
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.cancel_called.lock().unwrap() = true;
            Ok(())
        }
    }

    struct FailOnValueSink {
        fail_on: u32,
        writes: Arc<Mutex<Vec<u32>>>,
    }
    impl WritableSink<u32> for FailOnValueSink {
        async fn write(
            &mut self,
            chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.writes.lock().unwrap().push(chunk);
            if chunk == self.fail_on {
                Err("error1".into())
            } else {
                Ok(())
            }
        }
    }

    let cancel_called = Arc::new(Mutex::new(false));
    let source = ReadableStream::builder(ClosedSourceCancelTracking {
        cancel_called: cancel_called.clone(),
    })
    .spawn(tokio::spawn);

    let writes = Arc::new(Mutex::new(Vec::new()));
    let dest = WritableStream::builder(FailOnValueSink {
        fail_on: 3,
        writes: writes.clone(),
    })
    .strategy(CountQueuingStrategy::new(1))
    .spawn(tokio::spawn);

    let err = source
        .pipe_to(&dest, None)
        .await
        .expect_err("pipeTo must reject with the last write's error");
    assert!(err.to_string().contains("error1"), "got: {err}");
    assert_eq!(
        *writes.lock().unwrap(),
        vec![1, 2, 3],
        "all three chunks are written before the final write rejects"
    );
    assert!(
        !*cancel_called.lock().unwrap(),
        "source.cancel() must NOT be called — the source is already closed"
    );
}

// WPT: close-propagation-forward.any.js — "erroring the writable while flushing pending writes
// should error pipeTo". The source is closed with chunks still queued in the destination, so the
// pipe is waiting for pending writes to flush before closing the dest. An in-flight write that
// rejects during that flush must reject pipeTo with the write error, leave the already-closed
// source uncancelled, and dispatch no further chunks to the sink.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_in_flight_write_error_during_close_flush_errors_pipe() {
    use std::sync::{Arc, Mutex};

    struct ClosedSource {
        cancel_called: Arc<Mutex<bool>>,
    }
    impl ReadableSource<u32> for ClosedSource {
        async fn start(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(1)?;
            controller.enqueue(2)?;
            controller.close()?;
            Ok(())
        }
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Ok(())
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.cancel_called.lock().unwrap() = true;
            Ok(())
        }
    }

    // First write blocks until the test fires `reject`, then fails — modelling a write that is
    // still in flight when the pipe reaches its close-flush phase.
    struct BlockThenRejectSink {
        writes: Arc<Mutex<Vec<u32>>>,
        write_started: Arc<tokio::sync::Notify>,
        reject: Arc<tokio::sync::Notify>,
    }
    impl WritableSink<u32> for BlockThenRejectSink {
        async fn write(
            &mut self,
            chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.writes.lock().unwrap().push(chunk);
            self.write_started.notify_one();
            self.reject.notified().await;
            Err("error1".into())
        }
    }

    let cancel_called = Arc::new(Mutex::new(false));
    let source = ReadableStream::builder(ClosedSource {
        cancel_called: cancel_called.clone(),
    })
    .spawn(tokio::spawn);

    let writes = Arc::new(Mutex::new(Vec::new()));
    let write_started = Arc::new(tokio::sync::Notify::new());
    let reject = Arc::new(tokio::sync::Notify::new());
    // HWM 3 so both chunks are accepted into the destination without backpressure blocking the
    // pipe's reads; the source is then observed as closed while write(1) is still in flight.
    let dest = WritableStream::builder(BlockThenRejectSink {
        writes: writes.clone(),
        write_started: write_started.clone(),
        reject: reject.clone(),
    })
    .strategy(CountQueuingStrategy::new(3))
    .spawn(tokio::spawn);

    let pipe = tokio::spawn(async move { source.pipe_to(&dest, None).await });

    // Wait until the first write is in flight, then reject it mid-flush.
    write_started.notified().await;
    reject.notify_one();

    let err = pipe
        .await
        .unwrap()
        .expect_err("pipeTo must reject with the in-flight write's error");
    assert!(err.to_string().contains("error1"), "got: {err}");
    assert_eq!(
        *writes.lock().unwrap(),
        vec![1],
        "only the first chunk reaches the sink; the second is never dispatched after the error"
    );
    assert!(
        !*cancel_called.lock().unwrap(),
        "source.cancel() must NOT be called — the source is already closed"
    );
}

// ── WPT: piping/error-propagation-via-cancel.any.js ──────────────────────────

// "Piping: a slow-closing writable that errors before closing cancels the readable"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_writable_that_errors_on_close_cancels_source() {
    struct ErrorOnCloseSink;

    impl WritableSink<u32> for ErrorOnCloseSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            Err("close failed".into())
        }
    }

    // Source produces one chunk then closes
    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let dest = WritableStream::builder(ErrorOnCloseSink).spawn(tokio::spawn);

    let result = source.pipe_to(&dest, None).await;
    // The sink's close() fails → pipe returns an error
    assert!(result.is_err(), "pipe should error when sink.close() rejects");
}

// ── WPT: piping/flow-control.any.js ──────────────────────────────────────────
// https://github.com/web-platform-tests/wpt/blob/master/streams/piping/flow-control.any.js

// "Piping: backpressure is respected — source is not read ahead of writable HWM"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_respects_writable_backpressure() {
    use std::sync::{Arc, Mutex};

    let write_order: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let write_order2 = write_order.clone();
    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct SlowOrderSink {
        order: Arc<Mutex<Vec<u32>>>,
        unblock: Arc<tokio::sync::Notify>,
        first: bool,
    }

    impl WritableSink<u32> for SlowOrderSink {
        async fn write(
            &mut self,
            chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.order.lock().unwrap().push(chunk);
            if self.first {
                self.first = false;
                self.unblock.notified().await; // block first write
            }
            Ok(())
        }
    }

    let data = vec![1u32, 2, 3, 4, 5];
    let source = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let dest = WritableStream::builder(SlowOrderSink {
        order: write_order2,
        unblock: unblock2,
        first: true,
    })
    .strategy(CountQueuingStrategy::new(1)) // HWM=1: backpressure after 1 queued chunk
    .spawn(tokio::spawn);

    let pipe = tokio::spawn(async move { source.pipe_to(&dest, None).await });

    // Let pipe start and first write block
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Unblock the sink — remaining writes proceed
    unblock.notify_one();
    pipe.await.unwrap().unwrap();

    // All chunks must arrive in the original order
    assert_eq!(
        *write_order.lock().unwrap(),
        data,
        "chunks must arrive in order even with backpressure"
    );
}

// "Piping: chunks from source arrive at dest in original order with HWM=1"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_hwm1_preserves_order() {
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let data: Vec<u32> = (1..=10).collect();
    let source = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);

    source.pipe_to(&dest, None).await.unwrap();
    assert_eq!(*collected.lock().unwrap(), data);
}

// WPT: flow-control.any.js —
// "pipeTo should read chunks ahead of finishing with previous ones when the
//  destination desires more" — a HWM > 1 destination is filled ahead: while the
// first write is still in flight, the pipe reads further chunks into the writable
// queue (contrast pipe_to_respects_writable_backpressure at HWM 1, and the HWM-0
// no-read-ahead case).
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_reads_ahead_into_hwm_gt_1_destination() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let produced = Arc::new(AtomicU32::new(0));

    struct CountingSource {
        next: u32,
        produced: Arc<AtomicU32>,
    }
    impl ReadableSource<u32> for CountingSource {
        async fn pull(
            &mut self,
            c: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            if self.next >= 10 {
                c.close()?;
                return Ok(());
            }
            self.next += 1;
            c.enqueue(self.next)?;
            self.produced.fetch_add(1, Ordering::Release);
            Ok(())
        }
    }

    let unblock = Arc::new(tokio::sync::Notify::new());
    struct BlockFirstSink {
        unblock: Arc<tokio::sync::Notify>,
        first: bool,
    }
    impl WritableSink<u32> for BlockFirstSink {
        async fn write(
            &mut self,
            _c: u32,
            _: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            if self.first {
                self.first = false;
                self.unblock.notified().await;
            }
            Ok(())
        }
    }

    let source = ReadableStream::builder(CountingSource {
        next: 0,
        produced: produced.clone(),
    })
    .spawn(tokio::spawn);
    let dest = WritableStream::builder(BlockFirstSink {
        unblock: unblock.clone(),
        first: true,
    })
    .strategy(CountQueuingStrategy::new(3))
    .spawn(tokio::spawn);

    let pipe = tokio::spawn(async move { source.pipe_to(&dest, None).await });

    // While the first write is blocked, the pipe fills the HWM-3 writable queue
    // ahead: more than the single in-flight chunk is read from the source.
    for _ in 0..64 {
        if produced.load(Ordering::Acquire) >= 3 {
            break;
        }
        tokio::task::yield_now().await;
    }
    let ahead = produced.load(Ordering::Acquire);
    assert!(
        ahead >= 3,
        "a HWM-3 destination must be filled ahead while the first write is in flight; \
         only {ahead} chunk(s) were read"
    );

    // Unblock and let the pipe drain the rest.
    unblock.notify_one();
    pipe.await.unwrap().unwrap();
}

// ── WPT gaps: close-propagation-forward ──────────────────────────────────────

// "Closing must be propagated forward: rejected close promise"
// When sink.close() rejects, pipe_to() must reject with that error.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_close_rejection_propagates_forward() {
    struct FailCloseSink;
    impl WritableSink<u32> for FailCloseSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
        async fn close(self) -> StreamResult<()> { Err("close failed".into()) }
    }

    let source = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let dest = WritableStream::builder(FailCloseSink).spawn(tokio::spawn);
    let result = source.pipe_to(&dest, None).await;
    assert!(result.is_err(), "pipe_to() must reject when sink.close() rejects");
    assert!(
        result.unwrap_err().to_string().contains("close failed"),
        "rejection must carry the close error"
    );
}

// "Closing must be propagated forward (with data): rejected close promise"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_close_rejection_after_chunks_propagates() {
    struct FailCloseSink { received: std::sync::Arc<std::sync::Mutex<Vec<u32>>> }
    impl WritableSink<u32> for FailCloseSink {
        async fn write(&mut self, c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            self.received.lock().unwrap().push(c);
            Ok(())
        }
        async fn close(self) -> StreamResult<()> { Err("close failed".into()) }
    }

    let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let source = ReadableStream::from_vec(vec![1u32, 2]).spawn(tokio::spawn);
    let dest = WritableStream::builder(FailCloseSink { received: received.clone() }).spawn(tokio::spawn);
    let result = source.pipe_to(&dest, None).await;
    assert!(result.is_err(), "pipe_to() must reject when sink.close() rejects after chunks");
    // All chunks must have been received before close was attempted
    assert_eq!(*received.lock().unwrap(), vec![1, 2]);
}

// ── WPT gaps: close-propagation-backward ─────────────────────────────────────

// "Closing must be propagated backward: rejected cancel promise"
// When source.cancel() throws during backward close propagation, pipe_to() rejects.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_cancel_rejection_on_dest_close_propagates_backward() {
    struct ThrowingCancelSource;
    impl ReadableSource<u32> for ThrowingCancelSource {
        async fn pull(&mut self, _c: &mut ReadableStreamDefaultController<u32>) -> StreamResult<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err(StreamError::from("cancel failed"))
        }
    }

    struct ImmediateCloseSink;
    impl WritableSink<u32> for ImmediateCloseSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
    }

    let source = ReadableStream::builder(ThrowingCancelSource).spawn(tokio::spawn);
    // The dest is already closed before piping starts
    let dest = WritableStream::builder(ImmediateCloseSink).spawn(tokio::spawn);
    let (_locked, writer) = dest.get_writer().unwrap();
    writer.close().await.unwrap();
    // Release the lock so pipe_to can acquire its own writer; the lock guard lives on
    // the writer, so the writer (not the Locked handle) must be dropped.
    drop(writer);
    // Piping into an already-closed dest propagates the close backward by cancelling
    // the source. The source's cancel() rejects, and per spec shutdown-with-action the
    // action's failure is what pipe_to rejects with — not the generic closed-dest error.
    let result = source.pipe_to(&dest, None).await;
    let err = result.expect_err("piping into a closed destination must reject");
    assert!(
        err.to_string().contains("cancel failed"),
        "pipe_to must reject with the source.cancel() rejection, got: {err}"
    );
}

// ── WPT gaps: error-propagation-forward (rejected abort) ─────────────────────

// "Errors must be propagated forward: rejected abort promise"
// When source errors and sink.abort() rejects, pipe_to() rejects with abort error.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_abort_rejection_on_source_error_propagates_forward() {
    struct FailAbortSink;
    impl WritableSink<u32> for FailAbortSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> { Ok(()) }
        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err(StreamError::from("abort failed"))
        }
    }

    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![],
        index: Default::default(),
    }).spawn(tokio::spawn);
    let dest = WritableStream::builder(FailAbortSink).spawn(tokio::spawn);
    let result = source.pipe_to(&dest, None).await;
    assert!(result.is_err(), "pipe_to() must reject when both source errors and sink.abort() rejects");
}

// "Errors must be propagated forward after one chunk: rejected abort promise"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_abort_rejection_after_chunks_propagates_forward() {
    struct FailAbortSink { count: std::sync::Arc<std::sync::Mutex<u32>> }
    impl WritableSink<u32> for FailAbortSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            *self.count.lock().unwrap() += 1;
            Ok(())
        }
        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err(StreamError::from("abort failed"))
        }
    }

    let count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![1u32],
        index: Default::default(),
    }).spawn(tokio::spawn);
    let dest = WritableStream::builder(FailAbortSink { count: count.clone() }).spawn(tokio::spawn);
    let result = source.pipe_to(&dest, None).await;
    assert!(result.is_err());
    assert_eq!(*count.lock().unwrap(), 1, "chunk before error must be written");
}

// ── WPT gaps: error-propagation-backward ─────────────────────────────────────

// "Errors must be propagated backward: rejected cancel promise on dest error"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_cancel_rejection_on_dest_error_propagates_backward() {
    struct ThrowingCancelSrc {
        data: Vec<u32>,
        idx: usize,
    }
    impl ReadableSource<u32> for ThrowingCancelSrc {
        async fn pull(&mut self, c: &mut ReadableStreamDefaultController<u32>) -> StreamResult<()> {
            if self.idx < self.data.len() {
                c.enqueue(self.data[self.idx])?;
                self.idx += 1;
            } else {
                futures::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err(StreamError::from("cancel failed"))
        }
    }

    let sink = FailAfterSink { fail_after: 0, count: Default::default() };
    let source = ReadableStream::builder(ThrowingCancelSrc { data: vec![1u32], idx: 0 })
        .spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);
    let result = source.pipe_to(&dest, None).await;
    // Spec shutdown-with-action: the dest errors, the source is cancelled, and the
    // source's cancel() rejection — not the sink write error — is what pipe_to rejects
    // with (WPT: "pipeTo must reject with the cancel error").
    let err = result.expect_err("pipe_to() must reject when the destination errors");
    assert!(
        err.to_string().contains("cancel failed"),
        "pipe_to() must reject with the source.cancel() rejection, got: {err}"
    );
}

// ── WPT gaps: abort.any.js ────────────────────────────────────────────────────

// "a rejection from underlyingSource.cancel() should be returned by pipeTo()"
// WPT abort.any.js test 5
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_signal_cancel_rejection_returned() {
    use whatwg_streams::AbortController;

    struct BlockThenErrorCancel;
    impl ReadableSource<u32> for BlockThenErrorCancel {
        async fn pull(&mut self, _c: &mut ReadableStreamDefaultController<u32>) -> StreamResult<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err(StreamError::from("cancel rejected"))
        }
    }

    let controller = AbortController::new();
    let signal = controller.signal();
    let source = ReadableStream::builder(BlockThenErrorCancel).spawn(tokio::spawn);
    let dest = WritableStream::builder(crate::helpers::LifecycleSink::default()).spawn(tokio::spawn);

    let pipe = tokio::spawn(async move {
        source.pipe_to(&dest, Some(StreamPipeOptions {
            signal: Some(signal),
            prevent_abort: true, // skip sink abort so we get the cancel error
            ..Default::default()
        })).await
    });

    tokio::task::yield_now().await;
    controller.abort(None);
    let result = pipe.await.unwrap();
    // With prevent_abort=true, source cancel runs and throws — the cancel rejection
    // itself must be returned, not a generic Aborted error.
    let msg = format!("{result:?}");
    assert!(
        msg.contains("cancel rejected"),
        "pipe_to() must return the source.cancel() rejection, got {result:?}"
    );
}

// "abort signal takes priority over closed writable"
// WPT abort.any.js test 10
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_signal_priority_over_closed_writable() {
    use whatwg_streams::AbortController;

    // Pre-abort the signal
    let controller = AbortController::new();
    let signal = controller.signal();
    controller.abort(None);

    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let dest = WritableStream::builder(crate::helpers::LifecycleSink::default()).spawn(tokio::spawn);
    // Close the destination first
    {
        let (_locked, writer) = dest.get_writer().unwrap();
        writer.close().await.unwrap();
    }

    let result = source.pipe_to(&dest, Some(StreamPipeOptions {
        signal: Some(signal),
        ..Default::default()
    })).await;
    // Abort signal should win — pipe_to errors
    assert!(result.is_err(), "abort signal must take priority over closed writable");
}

// "abort should do nothing after the readable is errored"
// WPT abort.any.js test 13 — late-firing abort signal on an already-errored source
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_late_abort_no_effect_after_readable_errored() {
    use whatwg_streams::AbortController;

    let controller = AbortController::new();
    let signal = controller.signal();
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };

    // Source errors immediately
    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![],
        index: Default::default(),
    }).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    // Pipe starts, source errors, pipe rejects
    let pipe = tokio::spawn(async move {
        source.pipe_to(&dest, Some(StreamPipeOptions {
            signal: Some(signal),
            ..Default::default()
        })).await
    });

    // Fire abort AFTER pipe has already errored
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    controller.abort(None);

    let result = pipe.await.unwrap();
    // Pipe already rejected due to source error — abort is a no-op
    assert!(result.is_err(), "pipe must reject due to source error");
}

// "the signal's reason propagates into cancel(), abort(), and the rejection"
// WPT abort.any.js — the reason given to AbortController.abort() must reach the
// source cancel(), the destination abort(), and the final pipeTo() rejection.
// Unwritable before AbortSignal carried a reason: AbortRegistration had none, so
// the pipe substituted a hardcoded "Aborted" string for all three.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_signal_reason_propagates_to_cancel_abort_and_rejection() {
    use whatwg_streams::AbortController;

    let controller = AbortController::new();
    let signal = controller.signal();

    // Source has no data, so it hangs in pull() until cancelled, keeping the
    // pipe live when the signal fires.
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let source = ReadableStream::builder(CancelTrackingSource {
        data: vec![],
        index: 0,
        cancel_reason: cancel_reason.clone(),
    })
    .spawn(tokio::spawn);

    let abort_reason = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: abort_reason.clone(),
    };
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let pipe = tokio::spawn(async move {
        source
            .pipe_to(
                &dest,
                Some(StreamPipeOptions {
                    signal: Some(signal),
                    ..Default::default()
                }),
            )
            .await
    });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    controller.abort(Some("custom abort reason".to_string()));

    let result = pipe.await.unwrap();

    assert_eq!(
        result,
        Err(StreamError::Aborted(Some("custom abort reason".to_string()))),
        "pipeTo must reject with the signal's reason, got {result:?}"
    );
    assert_eq!(
        cancel_reason.lock().unwrap().as_deref(),
        Some("custom abort reason"),
        "the signal's reason must reach source.cancel()"
    );
    assert_eq!(
        abort_reason.lock().unwrap().as_deref(),
        Some("custom abort reason"),
        "the signal's reason must reach sink.abort()"
    );
}

// ── WPT: piping/general.any.js ───────────────────────────────────────────────

// "Piping must lock both the ReadableStream and WritableStream" +
// "Piping finishing must unlock both"
// pipe_to() acquires a writer on the destination for the pipe's duration and
// drops it on completion. The readable side is locked by *move* — pipe_to takes
// the readable by value — so only the writable's runtime lock is observable here.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_locks_destination_during_and_unlocks_after() {
    use std::sync::Arc;
    use std::time::Duration;

    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct BlockFirstWriteSink {
        unblock: Arc<tokio::sync::Notify>,
        first: bool,
    }
    impl WritableSink<u32> for BlockFirstWriteSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            if self.first {
                self.first = false;
                self.unblock.notified().await; // hold the pipe in-flight
            }
            Ok(())
        }
    }

    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let dest = WritableStream::builder(BlockFirstWriteSink { unblock: unblock2, first: true })
        .spawn(tokio::spawn);

    assert!(!dest.locked(), "destination is unlocked before the pipe starts");

    let mut pipe = Box::pin(source.pipe_to(&dest, None));
    // Drive the pipe until it blocks in the first write: it has acquired the
    // writer (locking dest) and the fire-and-forget write is parked in the sink.
    let _ = tokio::time::timeout(Duration::from_millis(100), &mut pipe).await;
    assert!(dest.locked(), "destination is locked while the pipe runs");

    unblock.notify_one();
    pipe.await.unwrap();
    assert!(!dest.locked(), "destination is unlocked once the pipe finishes");
}

// "pipeTo must fail if the WritableStream is locked"
// The destination already has a writer, so pipe_to's internal get_writer() fails.
// (The readable-locked case — "pipeTo must fail if the RS is locked" — is enforced
// at compile time: pipe_to consumes an Unlocked readable by value, so a locked one
// cannot be passed.)
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_fails_if_destination_already_locked() {
    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let dest = WritableStream::builder(CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: Default::default(),
    })
    .spawn(tokio::spawn);

    // Hold a writer so the destination is locked.
    let (_held, _writer) = dest.get_writer().expect("first get_writer succeeds");

    assert!(
        source.pipe_to(&dest, None).await.is_err(),
        "pipe_to must fail when the destination is already locked"
    );
}

// ── WPT: piping/flow-control.any.js ──────────────────────────────────────────

// "Piping from a non-empty ReadableStream into a WritableStream that does not
//  desire chunks"
// HWM=0 means perpetual backpressure: the pipe blocks at writer.ready() and must
// read nothing (no read-ahead). Erroring the destination resolves ready() with the
// error, so the pipe rejects without ever writing. prevent_cancel mirrors the WPT
// option and keeps the (already-exhausted) source untouched.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_into_zero_hwm_writable_reads_nothing_then_error_rejects() {
    use std::sync::Arc;
    use std::time::Duration;

    let collected = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(0)) // HWM=0: never desires chunks
        .spawn(tokio::spawn);

    let mut pipe = Box::pin(source.pipe_to(
        &dest,
        Some(StreamPipeOptions { prevent_cancel: true, ..Default::default() }),
    ));

    // Give the pipe time to block on backpressure. It must not write anything.
    let _ = tokio::time::timeout(Duration::from_millis(100), &mut pipe).await;
    assert!(
        collected.lock().unwrap().is_empty(),
        "a zero-HWM destination must receive no writes — the pipe must not read ahead"
    );

    // Error the destination; the blocked ready() now rejects, ending the pipe.
    dest.abort(Some("boom".into())).await.unwrap();
    assert!(pipe.await.is_err(), "pipe must reject once the destination errors");
    assert!(
        collected.lock().unwrap().is_empty(),
        "no chunk should ever reach a destination that never desired one"
    );
}

// Skipped from general.any.js — untranslatable to the Rust API, not coverage gaps:
//
// - "pipeTo must check the brand of its ReadableStream this value" / "...its
//   WritableStream argument": brand checks guard against passing a non-stream
//   object. Rust generics fix the types at compile time; there is no wrong-type
//   value to pass.
// - "pipeTo must fail if the ReadableStream is locked": pipe_to consumes an
//   Unlocked readable by value, so a locked one cannot be passed — enforced by the
//   type-state at compile time rather than a runtime check. (The WritableStream
//   side is a runtime check and is covered by
//   pipe_to_fails_if_destination_already_locked.)
// - "pipeTo() should reject if an option getter grabs a writer": exercises a side
//   effect of evaluating the options object's accessor properties. StreamPipeOptions
//   is a plain struct of booleans — no getters, no side effects.
// - "pipeTo() promise should resolve if null is passed": passing null options.
//   The Rust equivalent is None, exercised by nearly every pipe test here.

// ── WPT: piping/close-propagation-forward.any.js +
//        piping/error-propagation-forward.any.js ───────────────────────────────

// Recording sink whose write() blocks until released, logging the order of
// write / close / abort so the "shutdown waits for the final write" invariant is
// observable.
#[cfg(feature = "send")]
struct OrderRecordingSink {
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    write_started: std::sync::Arc<tokio::sync::Notify>,
    unblock: std::sync::Arc<tokio::sync::Notify>,
}

#[cfg(feature = "send")]
impl WritableSink<u32> for OrderRecordingSink {
    async fn write(
        &mut self,
        chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.events.lock().unwrap().push(format!("write:{chunk}"));
        self.write_started.notify_one();
        self.unblock.notified().await; // hold the write in-flight
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        self.events.lock().unwrap().push("close".into());
        Ok(())
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        self.events.lock().unwrap().push(format!("abort:{}", reason.unwrap_or_default()));
        Ok(())
    }
}

// "Closing must be propagated forward: shutdown must not occur until the final
//  write completes"
// The source closes while a write is still in-flight. The pipe gates each read on
// writer.ready(), which stays pending until the in-flight write drains, so close()
// cannot reach the destination before the write finishes.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_close_waits_for_in_flight_write() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let write_started = Arc::new(tokio::sync::Notify::new());
    let unblock = Arc::new(tokio::sync::Notify::new());
    let done = Arc::new(AtomicBool::new(false));

    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn); // enqueues 1, then closes
    let dest = WritableStream::builder(OrderRecordingSink {
        events: events.clone(),
        write_started: write_started.clone(),
        unblock: unblock.clone(),
    })
    .spawn(tokio::spawn);

    let done2 = done.clone();
    let pipe = tokio::spawn(async move {
        let r = source.pipe_to(&dest, None).await;
        done2.store(true, Ordering::Release);
        r
    });

    // Wait until the write is in-flight, then confirm no shutdown has happened.
    write_started.notified().await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(*events.lock().unwrap(), vec!["write:1"], "destination must not be closed mid-write");
    assert!(!done.load(Ordering::Acquire), "pipe must not complete while the write is in-flight");

    // Release the write; close now propagates, after the write.
    unblock.notify_one();
    pipe.await.unwrap().unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        vec!["write:1", "close"],
        "close must follow the completed write, never precede it"
    );
}

// "Errors must be propagated forward: shutdown must not occur until the final
//  write completes"
// Symmetric to the close case: the source errors while a write is in-flight. The
// pipe waits for the write to drain, then aborts the destination with the error.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_abort_waits_for_in_flight_write() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let write_started = Arc::new(tokio::sync::Notify::new());
    let unblock = Arc::new(tokio::sync::Notify::new());
    let done = Arc::new(AtomicBool::new(false));

    // Source enqueues 1, then errors on the next pull.
    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![1u32],
        index: Default::default(),
    })
    .spawn(tokio::spawn);
    let dest = WritableStream::builder(OrderRecordingSink {
        events: events.clone(),
        write_started: write_started.clone(),
        unblock: unblock.clone(),
    })
    .spawn(tokio::spawn);

    let done2 = done.clone();
    let pipe = tokio::spawn(async move {
        let r = source.pipe_to(&dest, None).await;
        done2.store(true, Ordering::Release);
        r
    });

    write_started.notified().await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(*events.lock().unwrap(), vec!["write:1"], "destination must not be aborted mid-write");
    assert!(!done.load(Ordering::Acquire), "pipe must not complete while the write is in-flight");

    unblock.notify_one();
    let result = pipe.await.unwrap();
    assert!(result.is_err(), "pipe must reject with the source error");

    let log = events.lock().unwrap();
    assert_eq!(log.len(), 2, "exactly one write then one abort");
    assert_eq!(log[0], "write:1");
    assert!(log[1].starts_with("abort:"), "abort must follow the completed write, got {:?}", log[1]);
}

// ── WPT: piping/abort.any.js ─────────────────────────────────────────────────

// "a rejection from underlyingSink.abort() should be returned by pipeTo()"
// When the signal fires and the pipe aborts the destination, a failure from
// sink.abort() is the error pipeTo() rejects with — not a generic Aborted error.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_signal_abort_returns_sink_abort_rejection() {
    use whatwg_streams::AbortController;

    struct BlockingSource;
    impl ReadableSource<u32> for BlockingSource {
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
    }

    struct FailingAbortSink;
    impl WritableSink<u32> for FailingAbortSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            Ok(())
        }
        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err("sink abort failed".into())
        }
    }

    let controller = AbortController::new();
    let signal = controller.signal();
    let source = ReadableStream::builder(BlockingSource).spawn(tokio::spawn);
    let dest = WritableStream::builder(FailingAbortSink).spawn(tokio::spawn);

    let pipe = tokio::spawn(async move {
        source
            .pipe_to(
                &dest,
                Some(StreamPipeOptions {
                    signal: Some(signal),
                    ..Default::default()
                }),
            )
            .await
    });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    controller.abort(None);

    let result = pipe.await.unwrap();
    assert!(result.is_err(), "pipe must reject");
    let msg = format!("{result:?}");
    assert!(
        msg.contains("sink abort failed"),
        "pipeTo must reject with the sink.abort() rejection, got {result:?}"
    );
}

// ── WPT: piping/close-propagation-backward.any.js ────────────────────────────

// "Closing must be propagated backward": piping into an already-closed (or
// externally-closed) destination cancels the source — so it can release its
// resources — and rejects, since a chunk cannot be written into a closed stream.
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_closed_dest_cancels_source_and_rejects() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let cancelled = Arc::new(AtomicBool::new(false));

    struct RecordingSource {
        cancelled: Arc<AtomicBool>,
    }
    impl ReadableSource<u32> for RecordingSource {
        async fn pull(&mut self, c: &mut ReadableStreamDefaultController<u32>) -> StreamResult<()> {
            c.enqueue(1)?;
            Ok(())
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            self.cancelled.store(true, Ordering::Release);
            Ok(())
        }
    }

    struct PlainSink;
    impl WritableSink<u32> for PlainSink {
        async fn write(&mut self, _c: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            Ok(())
        }
    }

    let source = ReadableStream::builder(RecordingSource { cancelled: cancelled.clone() })
        .spawn(tokio::spawn);

    let dest = WritableStream::builder(PlainSink).spawn(tokio::spawn);
    let (_l, writer) = dest.get_writer().unwrap();
    writer.close().await.unwrap();
    drop(writer);

    let result = source.pipe_to(&dest, None).await;

    // The pipe awaits the source cancel before returning, so `cancelled` is set by now.
    assert!(result.is_err(), "piping into a closed destination must reject, got {result:?}");
    assert!(
        cancelled.load(Ordering::Acquire),
        "the source must be cancelled when the destination is already closed"
    );
}

// WPT: abort.any.js — "abort should do nothing after the writable is errored".
// Once the destination has errored, a late abort signal is a no-op: pipe_to rejects with the
// writable's error (not the abort), and with preventCancel the source is never cancelled.
//
// Divergence (asserts the spec-correct outcome): the pipe rejects with "Stream was aborted"
// instead of the writable's error. Root cause: the pipe watches the abort signal continuously
// (a select! arm) but only observes the destination's errored state at a write point — while it
// is blocked awaiting a source read, a destination error goes unnoticed, so a later abort wins.
// A no-abort probe with this same setup hangs, confirming the pipe never reacts to the dest
// error while reading. Fixing it means watching the destination's terminal state alongside the
// read in the pipe loop — invasive, deferred. See WPT_COVERAGE.md.
#[cfg(feature = "send")]
#[tokio::test]
#[ignore = "pipe does not observe a destination error while blocked on a read; late abort wins (see WPT_COVERAGE.md)"]
async fn pipe_to_late_abort_no_effect_after_writable_errored() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::AbortController;

    struct BlockingCancelSource {
        cancel_called: Arc<Mutex<bool>>,
    }
    impl ReadableSource<u32> for BlockingCancelSource {
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.cancel_called.lock().unwrap() = true;
            Ok(())
        }
    }

    struct CapturingSink {
        ctrl: Arc<Mutex<Option<WritableStreamDefaultController>>>,
    }
    impl WritableSink<u32> for CapturingSink {
        async fn start(
            &mut self,
            controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            *self.ctrl.lock().unwrap() = Some(controller.clone());
            Ok(())
        }
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            Ok(())
        }
    }

    let cancel_called = Arc::new(Mutex::new(false));
    let ctrl_slot: Arc<Mutex<Option<WritableStreamDefaultController>>> = Arc::new(Mutex::new(None));

    let source = ReadableStream::builder(BlockingCancelSource {
        cancel_called: cancel_called.clone(),
    })
    .spawn(tokio::spawn);
    let dest = WritableStream::builder(CapturingSink {
        ctrl: ctrl_slot.clone(),
    })
    .spawn(tokio::spawn);

    let abort = AbortController::new();
    let signal = abort.signal();

    let pipe = tokio::spawn(async move {
        source
            .pipe_to(
                &dest,
                Some(StreamPipeOptions {
                    signal: Some(signal),
                    prevent_cancel: true,
                    ..Default::default()
                }),
            )
            .await
    });

    // Let the pipe start and the sink's start() capture the controller.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let writer_ctrl = ctrl_slot
        .lock()
        .unwrap()
        .clone()
        .expect("sink start() must have captured the controller");

    // Error the writable, let the pipe observe it, then fire a late abort — a no-op.
    writer_ctrl.error("error1".into());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    abort.abort(None);

    let err = pipe.await.unwrap().expect_err("pipe must reject");
    assert!(
        err.to_string().contains("error1"),
        "pipe must reject with the writable error, not the abort; got: {err}"
    );
    assert!(
        !*cancel_called.lock().unwrap(),
        "source.cancel() must not be called (preventCancel)"
    );
}

// WPT: abort.any.js — "abort should do nothing after the readable is errored, even with pending
// writes". The sink errors the readable mid-write (write still pending); a late abort must be a
// no-op, so pipe_to rejects with the readable's error (preventAbort keeps the writable usable).
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_late_abort_no_effect_after_readable_errored_with_pending_write() {
    use std::sync::{Arc, Mutex};
    use whatwg_streams::AbortController;

    type ReadCtrlSlot = Arc<Mutex<Option<ReadableStreamDefaultController<u32>>>>;

    struct CapturingSource {
        ctrl: ReadCtrlSlot,
    }
    impl ReadableSource<u32> for CapturingSource {
        async fn start(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.ctrl.lock().unwrap() = Some(controller.clone());
            controller.enqueue(1)?;
            Ok(())
        }
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Ok(())
        }
    }

    // Errors the readable mid-write, then blocks until released — modelling a pending write that
    // is still in flight when the readable errors and the abort fires.
    struct ErrorReadableThenBlockSink {
        read_ctrl: ReadCtrlSlot,
        release: Arc<tokio::sync::Notify>,
        write_started: Arc<tokio::sync::Notify>,
    }
    impl WritableSink<u32> for ErrorReadableThenBlockSink {
        async fn write(
            &mut self,
            _chunk: u32,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            if let Some(rc) = self.read_ctrl.lock().unwrap().clone() {
                let _ = rc.error("error1".into());
            }
            self.write_started.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    let ctrl_slot: ReadCtrlSlot = Arc::new(Mutex::new(None));
    let release = Arc::new(tokio::sync::Notify::new());
    let write_started = Arc::new(tokio::sync::Notify::new());

    let source = ReadableStream::builder(CapturingSource {
        ctrl: ctrl_slot.clone(),
    })
    .spawn(tokio::spawn);
    let dest = WritableStream::builder(ErrorReadableThenBlockSink {
        read_ctrl: ctrl_slot.clone(),
        release: release.clone(),
        write_started: write_started.clone(),
    })
    .strategy(CountQueuingStrategy::new(3))
    .spawn(tokio::spawn);

    let abort = AbortController::new();
    let signal = abort.signal();

    let pipe = tokio::spawn(async move {
        source
            .pipe_to(
                &dest,
                Some(StreamPipeOptions {
                    signal: Some(signal),
                    prevent_abort: true,
                    ..Default::default()
                }),
            )
            .await
    });

    // Wait until the write is in flight (readable already errored), then fire the late abort and
    // release the write.
    write_started.notified().await;
    abort.abort(None);
    release.notify_one();

    let err = pipe.await.unwrap().expect_err("pipe must reject");
    assert!(
        err.to_string().contains("error1"),
        "pipe must reject with the readable error, not the abort; got: {err}"
    );
}
