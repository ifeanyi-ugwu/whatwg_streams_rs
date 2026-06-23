// WPT: streams/readable-streams/tee.any.js

use whatwg_streams::{
    BackpressureMode, ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamError,
    StreamResult,
};

struct ErroringSource {
    emitted: std::sync::Arc<std::sync::Mutex<u32>>,
    max: u32,
}

impl ReadableSource<u32> for ErroringSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        let mut n = self.emitted.lock().unwrap();
        if *n < self.max {
            *n += 1;
            let v = *n;
            drop(n);
            controller.enqueue(v)?;
        } else {
            return Err(StreamError::from("source error"));
        }
        Ok(())
    }
}

// "ReadableStream tee() returns two readable streams"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_returns_two_streams() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();
    drop(r1);
    drop(r2);
}

// "ReadableStream tee(): both branches receive all chunks"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_both_branches_get_all_chunks() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (branch1, branch2) = stream
        .tee()
        .backpressure_mode(BackpressureMode::SpecCompliant)
        .spawn(tokio::spawn)
        .unwrap();

    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let mut b1 = Vec::new();
    while let Some(v) = r1.read().await.unwrap() {
        b1.push(v);
    }
    let mut b2 = Vec::new();
    while let Some(v) = r2.read().await.unwrap() {
        b2.push(v);
    }

    assert_eq!(b1, data);
    assert_eq!(b2, data);
}

// "ReadableStream tee(): chunks are equal across branches"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_chunks_are_cloned_independently() {
    let stream = ReadableStream::from_vec(vec![42u32]).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();
    assert_eq!(r1.read().await.unwrap(), Some(42));
    assert_eq!(r2.read().await.unwrap(), Some(42));
}

// "ReadableStream tee(): cancelling branch1 does not stop branch2 from reading"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_cancelling_one_branch_does_not_affect_the_other() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    r1.cancel(Some("branch1 done".into())).await.unwrap();

    let mut collected = Vec::new();
    while let Some(v) = r2.read().await.unwrap() {
        collected.push(v);
    }
    assert_eq!(collected, data);
}

// "ReadableStream tee(): cancelling both branches cancels the original"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_cancelling_both_branches_cancels_original() {
    use std::sync::{Arc, Mutex};

    let cancel_count = Arc::new(Mutex::new(0u32));
    let cancel_count2 = cancel_count.clone();

    struct TrackingSource {
        cancel_count: Arc<Mutex<u32>>,
    }

    impl ReadableSource<u32> for TrackingSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(1)?;
            controller.enqueue(2)?;
            controller.enqueue(3)?;
            Ok(())
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            *self.cancel_count.lock().unwrap() += 1;
            Ok(())
        }
    }

    let stream = ReadableStream::builder(TrackingSource {
        cancel_count: cancel_count2,
    })
    .spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    r1.cancel(None).await.unwrap();
    r2.cancel(None).await.unwrap();

    // Both branches cancelled → source cancel() must have been called exactly once
    assert_eq!(
        *cancel_count.lock().unwrap(),
        1,
        "source cancel() must be invoked exactly once when both tee branches cancel"
    );
}

// "ReadableStream tee(): error from source propagates to both branches"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_source_error_propagates_to_both_branches() {
    use std::sync::{Arc, Mutex};
    let emitted = Arc::new(Mutex::new(0u32));
    let source = ErroringSource {
        emitted: emitted.clone(),
        max: 1,
    };
    let stream = ReadableStream::builder(source).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    assert_eq!(r1.read().await.unwrap(), Some(1));
    let err1 = r1.read().await.expect_err("r1 should receive the source error");

    // r2 may deliver the buffered chunk before the error, or error immediately
    let err2 = match r2.read().await {
        Ok(Some(_)) => r2.read().await.expect_err("r2 second read should be the source error"),
        Err(e) => e,
        Ok(None) => panic!("expected source error, got EOF on r2"),
    };

    assert_eq!(
        err1.to_string(),
        err2.to_string(),
        "both tee branches must receive the same source error"
    );
}

// "ReadableStream tee(): SlowestConsumer mode requires concurrent consumption"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_slowest_consumer_mode() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (branch1, branch2) = stream
        .tee()
        .backpressure_mode(BackpressureMode::SlowestConsumer)
        .spawn(tokio::spawn)
        .unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let (b1, b2) = tokio::join!(
        async {
            let mut v = Vec::new();
            while let Some(x) = r1.read().await.unwrap() {
                v.push(x);
            }
            v
        },
        async {
            let mut v = Vec::new();
            while let Some(x) = r2.read().await.unwrap() {
                v.push(x);
            }
            v
        }
    );

    assert_eq!(b1, data);
    assert_eq!(b2, data);
}

// "ReadableStream tee(): prepare() exposes futures for manual driving"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_prepare_without_spawn() {
    let stream = ReadableStream::from_vec(vec![10u32, 20]).spawn(tokio::spawn);
    let (branch1, branch2, coord_fut, rfut1, rfut2) = stream.tee().prepare().unwrap();
    tokio::spawn(async move { futures::join!(coord_fut, rfut1, rfut2) });

    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    assert_eq!(r1.read().await.unwrap(), Some(10));
    assert_eq!(r1.read().await.unwrap(), Some(20));
    assert_eq!(r1.read().await.unwrap(), None);
    assert_eq!(r2.read().await.unwrap(), Some(10));
    assert_eq!(r2.read().await.unwrap(), Some(20));
    assert_eq!(r2.read().await.unwrap(), None);
}

// "ReadableStream tee(): spawn_parts() separates coordinator and branches"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_spawn_parts() {
    let data = vec![5u32, 6, 7];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (branch1, branch2) = stream
        .tee()
        .spawn_parts(tokio::spawn, tokio::spawn, tokio::spawn)
        .unwrap();

    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let mut b1 = Vec::new();
    while let Some(v) = r1.read().await.unwrap() {
        b1.push(v);
    }
    let mut b2 = Vec::new();
    while let Some(v) = r2.read().await.unwrap() {
        b2.push(v);
    }
    assert_eq!(b1, data);
    assert_eq!(b2, data);
}

// ── WPT gaps (tee.any.js) ─────────────────────────────────────────────────────

// "ReadableStream teeing: canceling branch2 should not impact branch1"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_cancelling_branch2_does_not_affect_branch1() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    r2.cancel(Some("branch2 done".into())).await.unwrap();

    let mut collected = Vec::new();
    while let Some(v) = r1.read().await.unwrap() {
        collected.push(v);
    }
    assert_eq!(collected, data);
}

// "ReadableStream teeing: closing the original should close the branches"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_closing_source_closes_both_branches() {
    struct FiniteSource {
        items: Vec<u32>,
        idx: usize,
    }

    impl ReadableSource<u32> for FiniteSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            if self.idx < self.items.len() {
                controller.enqueue(self.items[self.idx])?;
                self.idx += 1;
            } else {
                controller.close()?;
            }
            Ok(())
        }
    }

    let stream = ReadableStream::builder(FiniteSource {
        items: vec![1u32, 2],
        idx: 0,
    })
    .spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let mut b1 = Vec::new();
    while let Some(v) = r1.read().await.unwrap() {
        b1.push(v);
    }
    let mut b2 = Vec::new();
    while let Some(v) = r2.read().await.unwrap() {
        b2.push(v);
    }
    assert_eq!(b1, vec![1, 2]);
    assert_eq!(b2, vec![1, 2]);
}

// "ReadableStream teeing: erroring the original should error both branches with the same error"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_erroring_source_errors_both_branches_with_same_error() {
    use std::sync::{Arc, Mutex};

    let emitted = Arc::new(Mutex::new(0u32));
    let source = ErroringSource {
        emitted: emitted.clone(),
        max: 0, // error immediately, no chunks
    };
    let stream = ReadableStream::builder(source).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let err1 = r1.read().await.expect_err("branch1 must get source error");
    let err2 = r2.read().await.expect_err("branch2 must get source error");

    assert_eq!(
        err1.to_string(),
        err2.to_string(),
        "both tee branches must receive the same error"
    );
}

// "ReadableStream teeing: canceling branch1 should finish when branch2 reads to end"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_cancel_branch1_resolves_when_branch2_exhausts_source() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let cancel = tokio::spawn(async move { r1.cancel(None).await });

    let mut collected = Vec::new();
    while let Some(v) = r2.read().await.unwrap() {
        collected.push(v);
    }
    assert_eq!(collected, data);

    cancel.await.unwrap().unwrap();
}

// "ReadableStream teeing: canceling branch1 should finish when original stream errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_cancel_branch1_resolves_when_source_errors() {
    use std::sync::{Arc, Mutex};

    let emitted = Arc::new(Mutex::new(0u32));
    let source = ErroringSource {
        emitted: emitted.clone(),
        max: 1, // one chunk then error
    };
    let stream = ReadableStream::builder(source).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    // Cancel branch1 — must resolve even though source later errors
    r1.cancel(None).await.unwrap();

    // Branch2 sees the chunk then the source error
    assert_eq!(r2.read().await.unwrap(), Some(1));
    assert!(r2.read().await.is_err(), "branch2 must see source error");
}

// "ReadableStream teeing: failing to cancel should propagate error to branches"
// The second branch cancel() waits for the coordinator to call source.cancel() and
// returns its result. At least one branch cancel must reject when source.cancel() throws.
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_failing_source_cancel_propagates_to_branch_cancel() {
    struct ThrowingCancelSource;

    impl ReadableSource<u32> for ThrowingCancelSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(1)?;
            controller.enqueue(2)?;
            controller.enqueue(3)?;
            Ok(())
        }

        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Err(StreamError::from("cancel threw"))
        }
    }

    let stream = ReadableStream::builder(ThrowingCancelSource).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let r1_cancel = r1.cancel(None).await;
    let r2_cancel = r2.cancel(None).await;

    // Source cancel() throws — the error must propagate to at least one branch
    assert!(
        r1_cancel.is_err() || r2_cancel.is_err(),
        "source cancel() error must reach at least one branch cancel: \
         r1={r1_cancel:?} r2={r2_cancel:?}"
    );
}
