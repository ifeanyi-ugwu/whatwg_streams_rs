/// Test that send feature works with tokio::spawn (requires Send)
/// This test will FAIL to compile if LocalBoxFuture breaks Send requirements

#[cfg(feature = "send")]
#[tokio::test]
async fn test_send_feature_with_tokio_spawn() {
    use whatwg_streams::ReadableStream;

    // tokio::spawn requires Send, so if LocalBoxFuture breaks Send, this won't compile
    let stream = ReadableStream::from_vec(vec![1, 2, 3])
        .spawn(tokio::spawn);  // This requires Send!

    let (_, mut reader) = stream.get_reader().unwrap();

    let mut result = Vec::new();
    while let Some(val) = reader.read().await.unwrap() {
        result.push(val);
    }

    assert_eq!(result, vec![1, 2, 3]);
}

#[cfg(feature = "local")]
#[tokio::test]
async fn test_local_feature_with_spawn_local() {
    use whatwg_streams::ReadableStream;
    use tokio::task::LocalSet;

    let local = LocalSet::new();

    local.run_until(async {
        // spawn_local does NOT require Send
        let stream = ReadableStream::from_vec(vec![1, 2, 3])
            .spawn(tokio::task::spawn_local);

        let (_, mut reader) = stream.get_reader().unwrap();

        let mut result = Vec::new();
        while let Some(val) = reader.read().await.unwrap() {
            result.push(val);
        }

        assert_eq!(result, vec![1, 2, 3]);
    }).await;
}
