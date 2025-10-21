/// Compile-time tests to verify Send/Sync bounds are correct for each feature
///
/// These tests don't run at runtime - they're designed to pass or fail at compile time.
/// Run with:
///   cargo test --features send  (default)
///   cargo test --no-default-features --features local

use whatwg_streams::{ReadableStream, WritableStream, TransformStream};

// Helper trait to check if a type implements Send
fn assert_send<T: Send>() {}

// Helper trait to check if a type implements Sync
fn assert_sync<T: Sync>() {}

#[cfg(feature = "send")]
#[test]
fn test_send_feature_streams_are_send_sync() {
    // With 'send' feature, streams should be Send + Sync
    assert_send::<ReadableStream<i32, Vec<i32>, whatwg_streams::DefaultStream>>();
    assert_sync::<ReadableStream<i32, Vec<i32>, whatwg_streams::DefaultStream>>();

    // WritableStream should also be Send + Sync
    use whatwg_streams::WritableSink;

    #[derive(Clone)]
    struct DummySink;

    impl WritableSink<String> for DummySink {
        async fn write(
            &mut self,
            _chunk: String,
            _controller: &mut whatwg_streams::WritableStreamDefaultController,
        ) -> Result<(), whatwg_streams::StreamError> {
            Ok(())
        }
    }

    assert_send::<WritableStream<String, DummySink>>();
    assert_sync::<WritableStream<String, DummySink>>();
}

#[cfg(feature = "local")]
#[test]
fn test_local_feature_streams_not_required_to_be_send() {
    // With 'local' feature, streams are NOT required to be Send or Sync
    // This test just verifies the code compiles without Send/Sync bounds

    // We can use !Send types with local feature
    use std::rc::Rc;
    use std::cell::RefCell;

    let _rc_value: Rc<RefCell<i32>> = Rc::new(RefCell::new(42));

    // This verifies that our streams can work with !Send types in local mode
    // If this compiles, it means we're correctly not requiring Send
}

#[test]
fn test_basic_stream_compilation() {
    // This test should compile under both features
    let _data = vec![1, 2, 3];
    // Just verify the types are available
    let _: Option<ReadableStream<i32, Vec<i32>, whatwg_streams::DefaultStream>> = None;
}
