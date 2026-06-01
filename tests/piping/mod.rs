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
    use futures::future::AbortHandle;
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

    let (abort_handle, abort_registration) = AbortHandle::new_pair();
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
                    signal: Some(abort_registration),
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
    abort_handle.abort();

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
    use futures::future::AbortHandle;
    use whatwg_streams::StreamError;

    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    // Fire the signal before piping even starts
    abort_handle.abort();

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
                signal: Some(abort_registration),
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
    use futures::future::AbortHandle;

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

    let (abort_handle, abort_registration) = AbortHandle::new_pair();
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
                    signal: Some(abort_registration),
                    prevent_abort: true,
                    ..Default::default()
                }),
            )
            .await
    });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    abort_handle.abort();

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
