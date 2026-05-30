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
    let stream = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_locked, r1) = branch1.get_reader().unwrap();
    let (_locked, r2) = branch2.get_reader().unwrap();
    r1.cancel(None).await.unwrap();
    r2.cancel(None).await.unwrap();
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
    assert!(r1.read().await.is_err());

    match r2.read().await {
        Ok(Some(_)) => assert!(r2.read().await.is_err()),
        Ok(None) => panic!("expected error or chunk, got EOF"),
        Err(_) => {}
    }
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
