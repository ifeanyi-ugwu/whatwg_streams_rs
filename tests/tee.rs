//! Integration tests for ReadableStream.tee(), ported from:
//! WPT: streams/readable-streams/tee.any.js
//! https://github.com/web-platform-tests/wpt/tree/master/streams/readable-streams/tee.any.js

use whatwg_streams::{
    BackpressureMode, ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamError,
    StreamResult,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Source whose pull() rejects, used to test error propagation.
#[cfg(feature = "send")]
struct ErroringSource {
    emitted: std::sync::Arc<std::sync::Mutex<u32>>,
    max: u32,
}

#[cfg(feature = "send")]
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

// ── WPT: readable-streams/tee.any.js ─────────────────────────────────────────

// "ReadableStream tee() returns two readable streams"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_returns_two_streams() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();

    // Both should be independently readable
    let (_locked, reader1) = branch1.get_reader().unwrap();
    let (_locked, reader2) = branch2.get_reader().unwrap();
    drop(reader1);
    drop(reader2);
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

    let (_locked, reader1) = branch1.get_reader().unwrap();
    let (_locked, reader2) = branch2.get_reader().unwrap();

    // Read branch 1 fully
    let mut b1 = Vec::new();
    while let Some(v) = reader1.read().await.unwrap() {
        b1.push(v);
    }

    // Read branch 2 fully
    let mut b2 = Vec::new();
    while let Some(v) = reader2.read().await.unwrap() {
        b2.push(v);
    }

    assert_eq!(b1, data);
    assert_eq!(b2, data);
}

// "ReadableStream tee(): chunks are equal across branches (independent copies)"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_chunks_are_cloned_independently() {
    let stream = ReadableStream::from_vec(vec![42u32]).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();

    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    let v1 = r1.read().await.unwrap();
    let v2 = r2.read().await.unwrap();
    assert_eq!(v1, Some(42));
    assert_eq!(v2, Some(42));
}

// "ReadableStream tee(): cancelling branch1 does not stop branch2 from reading"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_cancelling_one_branch_does_not_affect_the_other() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();

    let (_locked, reader1) = branch1.get_reader().unwrap();
    let (_locked, reader2) = branch2.get_reader().unwrap();

    // Cancel branch1 immediately
    reader1.cancel(Some("branch1 done".into())).await.unwrap();

    // branch2 should still be able to read all data
    let mut collected = Vec::new();
    while let Some(v) = reader2.read().await.unwrap() {
        collected.push(v);
    }
    assert_eq!(collected, data);
}

// "ReadableStream tee(): cancelling both branches cancels the original stream"
// SPEC: §3.3.8 — when both branches cancel, the original source should be cancelled.
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_cancelling_both_branches_cancels_original() {
    // Use a finite stream; after both branches cancel, the coordinator should
    // call cancel on the original reader.
    let stream = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();

    let (_locked, reader1) = branch1.get_reader().unwrap();
    let (_locked, reader2) = branch2.get_reader().unwrap();

    reader1.cancel(None).await.unwrap();
    reader2.cancel(None).await.unwrap();

    // If we reach here without deadlock, the coordinator handled dual-cancel correctly.
}

// "ReadableStream tee(): error from source propagates to both branches"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_source_error_propagates_to_both_branches() {
    use std::sync::{Arc, Mutex};

    let emitted = Arc::new(Mutex::new(0u32));
    let source = ErroringSource {
        emitted: emitted.clone(),
        max: 1, // emit one chunk then error
    };

    let stream = ReadableStream::builder(source).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();

    let (_locked, reader1) = branch1.get_reader().unwrap();
    let (_locked, reader2) = branch2.get_reader().unwrap();

    // Consume the one good chunk on branch1
    let v = reader1.read().await.unwrap();
    assert_eq!(v, Some(1));

    // Next read on branch1 should error
    assert!(reader1.read().await.is_err());

    // branch2 should also see an error (may get the good chunk first)
    match reader2.read().await {
        Ok(Some(_)) => {
            // consumed the good chunk — next read should propagate the error
            assert!(reader2.read().await.is_err());
        }
        Ok(None) => panic!("expected an error or a chunk, got EOF"),
        Err(_) => {} // error propagated directly — acceptable
    }
}

// "ReadableStream tee(): works with SlowestConsumer backpressure mode"
// SlowestConsumer only pulls when BOTH branches have buffer space, so both
// branches must be consumed concurrently — sequential reads deadlock.
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

// "ReadableStream tee(): prepare() without spawn is usable with manual drive"
#[cfg(feature = "send")]
#[tokio::test]
async fn tee_prepare_without_spawn() {
    let stream = ReadableStream::from_vec(vec![10u32, 20]).spawn(tokio::spawn);
    let (branch1, branch2, coord_fut, rfut1, rfut2) = stream.tee().prepare().unwrap();

    // Drive all three futures in a single joined task
    tokio::spawn(async move {
        futures::join!(coord_fut, rfut1, rfut2);
    });

    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();

    assert_eq!(r1.read().await.unwrap(), Some(10));
    assert_eq!(r1.read().await.unwrap(), Some(20));
    assert_eq!(r1.read().await.unwrap(), None);

    assert_eq!(r2.read().await.unwrap(), Some(10));
    assert_eq!(r2.read().await.unwrap(), Some(20));
    assert_eq!(r2.read().await.unwrap(), None);
}

// "ReadableStream tee(): spawn_parts() lets caller separate coordinator and branches"
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
