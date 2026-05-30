// WPT: streams/readable-streams/general.any.js

use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamError, StreamResult,
};

struct FailingStartSource;

impl ReadableSource<u32> for FailingStartSource {
    async fn start(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        Err(StreamError::from("start rejected"))
    }

    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        Ok(())
    }
}

struct FailingPullSource;

impl ReadableSource<u32> for FailingPullSource {
    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        Err(StreamError::from("pull rejected"))
    }
}

// "ReadableStream: reading from an empty stream gives undefined"
#[cfg(feature = "send")]
#[tokio::test]
async fn reading_from_empty_stream_gives_none() {
    let stream = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableStream: reading all values yields each in order, then closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn reading_all_values_in_order_then_closed() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    for expected in &data {
        assert_eq!(reader.read().await.unwrap(), Some(*expected));
    }
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableStream: locked reflects whether a reader is held"
#[cfg(feature = "send")]
#[tokio::test]
async fn locked_reflects_reader_state() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    assert!(!stream.locked());
    let (_locked_stream, reader) = stream.get_reader().unwrap();
    assert!(stream.locked());
    let stream = reader.release_lock();
    assert!(!stream.locked());
}

// "ReadableStream: get_reader() on an already-locked stream returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_reader_fails_when_locked() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let (_locked, _reader) = stream.get_reader().unwrap();
    assert!(stream.get_reader().is_err());
}

// "ReadableStream: dropping the reader releases the lock"
#[cfg(feature = "send")]
#[tokio::test]
async fn dropping_reader_releases_lock() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    {
        let (_locked, _reader) = stream.get_reader().unwrap();
        assert!(stream.locked());
    }
    assert!(!stream.locked());
}

// "ReadableStream: after releasing a reader, a new reader can be acquired"
#[cfg(feature = "send")]
#[tokio::test]
async fn new_reader_after_release_lock() {
    let stream = ReadableStream::from_vec(vec![1u32, 2]).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(1));
    let stream = reader.release_lock();
    let (_locked, reader2) = stream.get_reader().unwrap();
    assert_eq!(reader2.read().await.unwrap(), Some(2));
}

// "ReadableStream: if start() rejects, the stream becomes errored"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_rejection_errors_stream() {
    let stream = ReadableStream::builder(FailingStartSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
}

// "ReadableStream: if pull() rejects, the stream becomes errored"
#[cfg(feature = "send")]
#[tokio::test]
async fn pull_rejection_errors_stream() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
}

// "ReadableStream: subsequent reads after error also return an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn reads_after_error_also_error() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
    assert!(reader.read().await.is_err());
}

// "ReadableStream: closed promise resolves after the stream closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn closed_resolves_when_stream_exhausted() {
    let stream = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await;
    reader.closed().await.unwrap();
}

// "ReadableStream: closed promise rejects when the stream errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn closed_rejects_when_stream_errored() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await;
    assert!(reader.closed().await.is_err());
}

// "ReadableStream: can be used as a futures::Stream"
#[cfg(feature = "send")]
#[tokio::test]
async fn implements_futures_stream_trait() {
    use futures::StreamExt;
    let items = vec![10u32, 20, 30];
    let mut stream = ReadableStream::from_vec(items.clone()).spawn(tokio::spawn);
    let mut collected = Vec::new();
    while let Some(result) = stream.next().await {
        collected.push(result.unwrap());
    }
    assert_eq!(collected, items);
}

// ── controller.error() ────────────────────────────────────────────────────────

// "ReadableStreamDefaultController: error() transitions stream to errored state"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_error_puts_stream_in_errored_state() {
    struct ErroringSource;

    impl ReadableSource<u32> for ErroringSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.error("controller error".into())?;
            Ok(())
        }
    }

    let stream = ReadableStream::builder(ErroringSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
}

// "ReadableStreamDefaultController: error() rejects all pending reads"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_error_rejects_pending_read() {
    use std::sync::{Arc, Mutex};

    // Source blocks in pull so we can have a genuinely pending read
    let fire = Arc::new(tokio::sync::Notify::new());
    let fire2 = fire.clone();
    let errored = Arc::new(Mutex::new(false));
    let errored2 = errored.clone();

    struct ControlledSource {
        fire: Arc<tokio::sync::Notify>,
        errored: Arc<Mutex<bool>>,
    }

    impl ReadableSource<u32> for ControlledSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            self.fire.notified().await;
            *self.errored.lock().unwrap() = true;
            controller.error("fired error".into())?;
            Ok(())
        }
    }

    let stream = ReadableStream::builder(ControlledSource {
        fire: fire2,
        errored: errored2,
    })
    .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    let read_fut = tokio::spawn(async move { reader.read().await });

    // Let the read reach pending state, then fire the error
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    fire.notify_one();

    let result = read_fut.await.unwrap();
    assert!(result.is_err(), "pending read should be rejected by controller.error()");
}

// "ReadableStreamDefaultController: error() makes closed promise reject"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_error_rejects_closed_promise() {
    struct ErroringSource;

    impl ReadableSource<u32> for ErroringSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.error("bad".into())?;
            Ok(())
        }
    }

    let stream = ReadableStream::builder(ErroringSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // trigger the error
    assert!(reader.closed().await.is_err());
}

// "ReadableStreamDefaultController: error() after close() is a no-op"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_error_after_close_is_noop() {
    struct CloseThenerrSource;

    impl ReadableSource<u32> for CloseThenerrSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.close()?;
            // Error after close — must be ignored by the implementation
            let _ = controller.error("too late".into());
            Ok(())
        }
    }

    let stream = ReadableStream::builder(CloseThenerrSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    // Stream should be closed, not errored
    assert_eq!(reader.read().await.unwrap(), None);
    reader.closed().await.unwrap();
}

// "ReadableStreamDefaultController: desired_size() is None when the stream is errored"
// The check must happen after the task processes ControllerMsg::Error (which sets the
// errored atomic). Cloning the controller shares the same Arc-backed atomics, so
// checking the clone after a failed read is reliable.
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_desired_size_is_none_after_error() {
    use std::sync::{Arc, Mutex};

    let captured_ctrl: Arc<Mutex<Option<ReadableStreamDefaultController<u32>>>> =
        Arc::new(Mutex::new(None));
    let captured2 = captured_ctrl.clone();

    struct CapturingErrorSource {
        captured: Arc<Mutex<Option<ReadableStreamDefaultController<u32>>>>,
    }

    impl ReadableSource<u32> for CapturingErrorSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // Clone shares Arc-backed atomics — reads correct value once task updates them
            *self.captured.lock().unwrap() = Some(controller.clone());
            controller.error("err".into())?;
            Ok(())
        }
    }

    let stream = ReadableStream::builder(CapturingErrorSource {
        captured: captured2,
    })
    .strategy(CountQueuingStrategy::new(4))
    .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // drives the task to process ControllerMsg::Error

    let ctrl = captured_ctrl.lock().unwrap().clone().unwrap();
    assert_eq!(
        ctrl.desired_size(),
        None,
        "desired_size() must be None when stream is errored"
    );
}

// ── start() sequence ──────────────────────────────────────────────────────────

// "ReadableStream: start() is called before pull()"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_is_called_before_pull() {
    use std::sync::{Arc, Mutex};

    let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let order2 = order.clone();

    struct OrderTrackingSource {
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ReadableSource<u32> for OrderTrackingSource {
        async fn start(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            self.order.lock().unwrap().push("start");
            Ok(())
        }

        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            self.order.lock().unwrap().push("pull");
            controller.close()?;
            Ok(())
        }
    }

    let stream = ReadableStream::builder(OrderTrackingSource { order: order2 }).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // drive to closed

    let calls = order.lock().unwrap().clone();
    assert_eq!(calls[0], "start", "start() must be called before pull()");
    assert!(calls.contains(&"pull"));
}

// "ReadableStream: start() can enqueue chunks via the controller"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_can_enqueue_via_controller() {
    struct EnqueueingStart;

    impl ReadableSource<u32> for EnqueueingStart {
        async fn start(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(42u32)?;
            controller.close()?;
            Ok(())
        }

        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Ok(())
        }
    }

    let stream = ReadableStream::builder(EnqueueingStart).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(42));
    assert_eq!(reader.read().await.unwrap(), None);
}

// ── reader.ready() ────────────────────────────────────────────────────────────

// "ReadableStream reader.ready() resolves when the queue has data"
#[cfg(feature = "send")]
#[tokio::test]
async fn reader_ready_resolves_when_queue_has_data() {
    let stream = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    // Give the task time to pull data into the queue
    tokio::task::yield_now().await;
    reader.ready().await.unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(1));
}

// "ReadableStream reader.ready() resolves immediately when stream is closed"
#[cfg(feature = "send")]
#[tokio::test]
async fn reader_ready_resolves_when_stream_is_closed() {
    let stream = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // drain to close
    reader.ready().await.unwrap(); // closed → resolves Ok
}

// "ReadableStream reader.ready() rejects when stream is errored"
#[cfg(feature = "send")]
#[tokio::test]
async fn reader_ready_rejects_when_stream_is_errored() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // trigger error
    assert!(reader.ready().await.is_err());
}

// "ReadableStream: desired_size reflects HWM minus queue depth"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_desired_size_reflects_hwm() {
    use std::sync::{Arc, Mutex};

    struct CapturingSource {
        desired_size: Arc<Mutex<Option<isize>>>,
    }

    impl ReadableSource<u32> for CapturingSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.desired_size.lock().unwrap() = controller.desired_size();
            controller.close()?;
            Ok(())
        }
    }

    let captured = Arc::new(Mutex::new(None));
    let stream = ReadableStream::builder(CapturingSource {
        desired_size: captured.clone(),
    })
    .strategy(CountQueuingStrategy::new(4))
    .spawn(tokio::spawn);

    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await;
    tokio::task::yield_now().await;

    assert_eq!(*captured.lock().unwrap(), Some(4));
}

// ── controller edge cases ─────────────────────────────────────────────────────

// "ReadableStreamDefaultController: calling close() twice is a no-op on the second call"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_close_twice_is_noop() {
    struct CloseTwiceSource;

    impl ReadableSource<u32> for CloseTwiceSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // First close should succeed; second should be ignored (not panic)
            let _ = controller.close();
            let _ = controller.close(); // no-op or silently ignored
            Ok(())
        }
    }

    let stream = ReadableStream::builder(CloseTwiceSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    // Stream should be closed, not errored
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableStreamDefaultController: enqueue() after close() throws" (spec §3.6.4)
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_enqueue_after_close_returns_error() {
    use std::sync::{Arc, Mutex};

    let enqueue_result: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let enqueue_result2 = enqueue_result.clone();

    struct CloseThEnqueueSource {
        enqueue_result: Arc<Mutex<Option<bool>>>,
    }

    impl ReadableSource<u32> for CloseThEnqueueSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.close()?;
            // close() sets close_requested synchronously — enqueue must return Err now
            let ok = controller.enqueue(42u32).is_ok();
            *self.enqueue_result.lock().unwrap() = Some(ok);
            Ok(())
        }
    }

    let stream = ReadableStream::builder(CloseThEnqueueSource {
        enqueue_result: enqueue_result2,
    })
    .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // drive the task

    assert_eq!(
        *enqueue_result.lock().unwrap(),
        Some(false),
        "enqueue() after close() must return Err (spec §3.6.4 [[closeRequested]])"
    );
}

// "ReadableStreamDefaultController: enqueue() on an errored stream is ignored"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_enqueue_on_errored_stream_returns_error() {
    use std::sync::{Arc, Mutex};

    let enqueue_result: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
    let enqueue_result2 = enqueue_result.clone();

    struct ErrorThenEnqueueSource {
        enqueue_result: Arc<Mutex<Option<bool>>>,
    }

    impl ReadableSource<u32> for ErrorThenEnqueueSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // error() sets error_requested synchronously before sending ControllerMsg::Error,
            // so enqueue() in the same frame sees error_requested=true and returns Err.
            controller.error("boom".into())?;
            let ok = controller.enqueue(99u32).is_ok();
            *self.enqueue_result.lock().unwrap() = Some(ok);
            Ok(())
        }
    }

    let stream = ReadableStream::builder(ErrorThenEnqueueSource {
        enqueue_result: enqueue_result2,
    })
    .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let result = reader.read().await;
    assert!(result.is_err(), "read() on an errored stream must return Err");
    assert!(reader.read().await.is_err(), "subsequent reads must also return Err");

    assert_eq!(
        *enqueue_result.lock().unwrap(),
        Some(false),
        "enqueue() after error() must return Err (error_requested is set synchronously)"
    );
}

// "ReadableStream: pull() is not invoked while the queue is at or above the HWM"
#[cfg(feature = "send")]
#[tokio::test]
async fn pull_not_called_while_queue_full() {
    use std::sync::{Arc, Mutex};

    let pull_count = Arc::new(Mutex::new(0u32));
    let pull_count2 = pull_count.clone();

    struct CountingSource {
        pull_count: Arc<Mutex<u32>>,
        items: Vec<u32>,
        index: usize,
    }

    impl ReadableSource<u32> for CountingSource {
        async fn start(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // Pre-fill the queue to HWM in start() — no further pulls should happen
            // until the consumer drains below HWM.
            for &item in &self.items {
                controller.enqueue(item)?;
            }
            Ok(())
        }

        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.pull_count.lock().unwrap() += 1;
            if self.index < self.items.len() {
                controller.enqueue(self.items[self.index])?;
                self.index += 1;
            } else {
                controller.close()?;
            }
            Ok(())
        }
    }

    // HWM=2: start() enqueues 2 items — queue is at HWM, pull() must NOT be called
    let stream = ReadableStream::builder(CountingSource {
        pull_count: pull_count2,
        items: vec![1u32, 2],
        index: 0,
    })
    .strategy(CountQueuingStrategy::new(2))
    .spawn(tokio::spawn);

    // Give the task time to run start() (which pre-fills the queue to HWM=2)
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // The critical invariant: pull() must NOT fire while the queue is at HWM.
    assert_eq!(
        *pull_count.lock().unwrap(),
        0,
        "pull() must not be called before any reads when queue is pre-filled to HWM"
    );

    let (_locked, reader) = stream.get_reader().unwrap();

    // Both reads return the items enqueued by start()
    assert_eq!(reader.read().await.unwrap(), Some(1));
    assert_eq!(reader.read().await.unwrap(), Some(2));

    // After draining, pull() is correctly triggered (queue empty)
    let _ = reader.read().await;
    assert!(
        *pull_count.lock().unwrap() > 0,
        "pull() must fire after the queue is drained"
    );
}
