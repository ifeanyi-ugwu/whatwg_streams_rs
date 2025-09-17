use super::super::{CountQueuingStrategy, QueuingStrategy, Unlocked, errors::StreamError};
use super::{
    readable::{DefaultStream, ReadableSource, ReadableStream, ReadableStreamDefaultController},
    writable::{WritableSink, WritableStream, WritableStreamDefaultController},
};
use futures::{
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
        oneshot,
    },
    future::{self, Future},
    stream::StreamExt,
};
use std::rc::Rc;

pub type StreamResult<T> = Result<T, StreamError>;

/// Commands sent from the writable side to the transform task
#[derive(Debug)]
enum TransformCommand<I> {
    Write {
        chunk: I,
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Close {
        completion: oneshot::Sender<StreamResult<()>>,
    },
    Abort {
        reason: Option<String>,
        completion: oneshot::Sender<StreamResult<()>>,
    },
}

/// TransformStream connecting readable and writable sides
pub struct TransformStream<I, O> {
    readable: ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>,
    writable: WritableStream<I, TransformWritableSink<I>, Unlocked>,
}

impl<I: 'static, O: 'static> TransformStream<I, O> {
    /// Get the readable side
    pub fn readable(
        self,
    ) -> ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked> {
        self.readable
    }

    /// Get the writable side  
    pub fn writable(self) -> WritableStream<I, TransformWritableSink<I>, Unlocked> {
        self.writable
    }

    /// Split into both sides
    pub fn split(
        self,
    ) -> (
        ReadableStream<O, TransformReadableSource<O>, DefaultStream, Unlocked>,
        WritableStream<I, TransformWritableSink<I>, Unlocked>,
    ) {
        (self.readable, self.writable)
    }
}

impl<I: 'static, O: 'static> TransformStream<I, O> {
    /// Internal builder that wires everything up but does not spawn anything.
    fn new_inner<T>(
        transformer: T,
        writable_strategy: Box<dyn QueuingStrategy<I>>,
        readable_strategy: Box<dyn QueuingStrategy<O>>,
    ) -> (
        Self,
        futures::future::LocalBoxFuture<'static, ()>, // readable task
        futures::future::LocalBoxFuture<'static, ()>, // writable task
        futures::future::LocalBoxFuture<'static, ()>, // transform task
    )
    where
        T: Transformer<I, O> + 'static,
    {
        let (transform_tx, transform_rx) = unbounded::<TransformCommand<I>>();

        let readable_source = TransformReadableSource::new();
        let writable_sink = TransformWritableSink::new(transform_tx);

        let (readable, readable_fut) =
            ReadableStream::new_inner(readable_source, readable_strategy);
        let (writable, writable_fut) = WritableStream::new_inner(writable_sink, writable_strategy);

        let controller = TransformStreamDefaultController::new(
            readable.controller.clone(),
            writable.controller.clone(),
        );

        let transform_fut = Box::pin(transform_task(transformer, transform_rx, controller));

        (
            TransformStream { readable, writable },
            readable_fut,
            writable_fut,
            transform_fut,
        )
    }
}

/// Controller for transform operations
pub struct TransformStreamDefaultController<O> {
    readable_controller: Rc<ReadableStreamDefaultController<O>>,
    writable_controller: Rc<WritableStreamDefaultController>,
}

impl<O> TransformStreamDefaultController<O> {
    fn new(
        readable_controller: Rc<ReadableStreamDefaultController<O>>,
        writable_controller: Rc<WritableStreamDefaultController>,
    ) -> Self {
        Self {
            readable_controller,
            writable_controller,
        }
    }

    /// Enqueue to readable side
    pub fn enqueue(&self, chunk: O) -> StreamResult<()> {
        self.readable_controller.enqueue(chunk)
    }

    /// Errors both the readable and writable side of the transform stream
    pub fn error(&self, error: StreamError) -> StreamResult<()> {
        self.readable_controller.error(error.clone())?;
        self.writable_controller.error(error);
        Ok(())
    }

    /// Closes the readable side and errors the writable side of the stream
    pub fn terminate(&self) -> StreamResult<()> {
        self.readable_controller.close()?;
        self.writable_controller
            .error(StreamError::Custom("Terminated".into()));
        Ok(())
    }

    /// Get desired size to fill the readable side of the stream's internal queue
    pub fn desired_size(&self) -> Option<isize> {
        self.readable_controller.desired_size()
    }
}

/// Transformer trait
pub trait Transformer<I, O> {
    /// Called once when the transform stream is created
    fn start(
        &mut self,
        controller: &mut TransformStreamDefaultController<O>,
    ) -> impl Future<Output = StreamResult<()>> {
        let _ = controller;
        future::ready(Ok(()))
    }

    /// Called for each chunk written to the writable side
    fn transform(
        &mut self,
        chunk: I,
        controller: &mut TransformStreamDefaultController<O>,
    ) -> impl Future<Output = StreamResult<()>>;

    /// Called when the writable side is closed
    fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<O>,
    ) -> impl Future<Output = StreamResult<()>> {
        let _ = controller;
        future::ready(Ok(()))
    }
}

/// Readable source for the transform stream
pub struct TransformReadableSource<O> {
    _phantom: std::marker::PhantomData<O>,
}

impl<O> TransformReadableSource<O> {
    fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<O: 'static> ReadableSource<O> for TransformReadableSource<O> {
    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<O>,
    ) -> StreamResult<()> {
        Ok(())
    }
}

/// Writable sink
pub struct TransformWritableSink<I> {
    transform_tx: UnboundedSender<TransformCommand<I>>,
}

impl<I> TransformWritableSink<I> {
    fn new(transform_tx: UnboundedSender<TransformCommand<I>>) -> Self {
        Self { transform_tx }
    }
}

impl<I: 'static> WritableSink<I> for TransformWritableSink<I> {
    async fn write(
        &mut self,
        chunk: I,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.transform_tx
            .unbounded_send(TransformCommand::Write {
                chunk,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Transform stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Write operation canceled".into())))
    }

    async fn close(self) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.transform_tx
            .unbounded_send(TransformCommand::Close { completion: tx })
            .map_err(|_| StreamError::Custom("Transform stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Close operation canceled".into())))
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        let (tx, rx) = oneshot::channel();

        self.transform_tx
            .unbounded_send(TransformCommand::Abort {
                reason,
                completion: tx,
            })
            .map_err(|_| StreamError::Custom("Transform stream task dropped".into()))?;

        rx.await
            .unwrap_or_else(|_| Err(StreamError::Custom("Abort operation canceled".into())))
    }
}

/// Simple transform task
async fn transform_task<I, O, T>(
    mut transformer: T,
    mut transform_rx: UnboundedReceiver<TransformCommand<I>>,
    mut controller: TransformStreamDefaultController<O>,
) where
    T: Transformer<I, O>,
{
    // Call start
    if let Err(error) = transformer.start(&mut controller).await {
        let _ = controller.error(error);
        return;
    }

    // Process commands
    while let Some(cmd) = transform_rx.next().await {
        match cmd {
            TransformCommand::Write { chunk, completion } => {
                let result = transformer.transform(chunk, &mut controller).await;
                match result {
                    Ok(()) => {
                        let _ = completion.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = controller.error(error.clone());
                        let _ = completion.send(Err(error));
                        break; // stop processing further writes
                    }
                }
            }

            TransformCommand::Close { completion } => {
                let flush_result = transformer.flush(&mut controller).await;
                if let Err(error) = flush_result {
                    let _ = controller.error(error.clone());
                    let _ = completion.send(Err(error));
                } else {
                    let _ = controller.terminate();
                    let _ = completion.send(Ok(()));
                }
                break;
            }

            TransformCommand::Abort { reason, completion } => {
                let error =
                    StreamError::Custom(reason.unwrap_or("Transform stream aborted".into()).into());
                let _ = controller.error(error.clone());
                let _ = completion.send(Err(error));
                break;
            }
        }
    }
}

/// An identity transformer that passes chunks through unchanged.
pub struct IdentityTransformer<T> {
    _phantom: std::marker::PhantomData<T>,
}

impl<T> IdentityTransformer<T> {
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T> Transformer<T, T> for IdentityTransformer<T> {
    fn transform(
        &mut self,
        chunk: T,
        controller: &mut TransformStreamDefaultController<T>,
    ) -> impl Future<Output = StreamResult<()>> {
        let result = controller.enqueue(chunk);
        future::ready(result)
    }
}

pub struct TransformStreamBuilder<I, O, T> {
    transformer: T,
    writable_strategy: Box<dyn QueuingStrategy<I>>,
    readable_strategy: Box<dyn QueuingStrategy<O>>,
}

impl<I: 'static, O: 'static, T: Transformer<I, O> + 'static> TransformStreamBuilder<I, O, T> {
    fn new(transformer: T) -> Self {
        Self {
            transformer,
            writable_strategy: Box::new(CountQueuingStrategy::new(1)),
            readable_strategy: Box::new(CountQueuingStrategy::new(1)),
        }
    }

    pub fn writable_strategy<S: QueuingStrategy<I> + 'static>(mut self, s: S) -> Self {
        self.writable_strategy = Box::new(s);
        self
    }

    pub fn readable_strategy<S: QueuingStrategy<O> + 'static>(mut self, s: S) -> Self {
        self.readable_strategy = Box::new(s);
        self
    }

    /// Return stream + futures without spawning
    pub fn prepare(
        self,
    ) -> (
        TransformStream<I, O>,
        futures::future::LocalBoxFuture<'static, ()>,
        futures::future::LocalBoxFuture<'static, ()>,
        futures::future::LocalBoxFuture<'static, ()>,
    ) {
        TransformStream::new_inner(
            self.transformer,
            self.writable_strategy,
            self.readable_strategy,
        )
    }

    /// Spawn bundled into one task
    pub fn spawn<F, R>(self, spawn_fn: F) -> TransformStream<I, O>
    where
        F: FnOnce(futures::future::LocalBoxFuture<'static, ()>) -> R,
    {
        let (stream, rfut, wfut, tfut) = self.prepare();
        let fut = async move {
            futures::join!(rfut, wfut, tfut);
        };
        spawn_fn(Box::pin(fut));
        stream
    }

    pub fn spawn_ref<F, R>(self, spawn_fn: &'static F) -> TransformStream<I, O>
    where
        F: Fn(futures::future::LocalBoxFuture<'static, ()>) -> R,
    {
        let (stream, rfut, wfut, tfut) = self.prepare();
        let fut = async move {
            let _ = futures::join!(rfut, wfut, tfut);
        };
        spawn_fn(Box::pin(fut));
        stream
    }

    /// Spawn each part separately
    pub fn spawn_parts<F1, F2, F3>(self, rf: F1, wf: F2, tf: F3) -> TransformStream<I, O>
    where
        F1: Fn(futures::future::LocalBoxFuture<'static, ()>),
        F2: Fn(futures::future::LocalBoxFuture<'static, ()>),
        F3: Fn(futures::future::LocalBoxFuture<'static, ()>),
    {
        let (stream, rfut, wfut, tfut) = self.prepare();
        rf(rfut);
        wf(wfut);
        tf(tfut);
        stream
    }

    pub fn spawn_parts_ref(
        self,
        rf: &'static (dyn Fn(futures::future::LocalBoxFuture<'static, ()>)),
        wf: &'static (dyn Fn(futures::future::LocalBoxFuture<'static, ()>)),
        tf: &'static (dyn Fn(futures::future::LocalBoxFuture<'static, ()>)),
    ) -> TransformStream<I, O> {
        let (stream, rfut, wfut, tfut) = self.prepare();
        rf(rfut);
        wf(wfut);
        tf(tfut);
        stream
    }
}

impl<I: 'static, O: 'static> TransformStream<I, O> {
    /// Returns a builder for this transform stream
    pub fn builder<T: Transformer<I, O> + 'static>(
        transformer: T,
    ) -> TransformStreamBuilder<I, O, T> {
        TransformStreamBuilder::new(transformer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use std::time::Duration;
    use tokio::time::timeout;

    // Test transformer that converts strings to uppercase
    pub struct UppercaseTransformer;

    impl Transformer<String, String> for UppercaseTransformer {
        fn transform(
            &mut self,
            chunk: String,
            controller: &mut TransformStreamDefaultController<String>,
        ) -> impl Future<Output = StreamResult<()>> {
            let result = controller.enqueue(chunk.to_uppercase());
            future::ready(result)
        }
    }

    // Test transformer that multiplies numbers by 2
    pub struct DoubleTransformer;

    impl Transformer<i32, i32> for DoubleTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> {
            let result = controller.enqueue(chunk * 2);
            future::ready(result)
        }
    }

    // Test transformer that filters out even numbers and passes odd ones
    pub struct OddFilterTransformer;

    impl Transformer<i32, i32> for OddFilterTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> {
            let result = if chunk % 2 != 0 {
                controller.enqueue(chunk)
            } else {
                Ok(()) // Filter out even numbers
            };
            future::ready(result)
        }
    }

    // Test transformer that errors on specific input
    pub struct ErrorOnThreeTransformer;

    impl Transformer<i32, i32> for ErrorOnThreeTransformer {
        fn transform(
            &mut self,
            chunk: i32,
            controller: &mut TransformStreamDefaultController<i32>,
        ) -> impl Future<Output = StreamResult<()>> {
            if chunk == 3 {
                future::ready(Err(StreamError::Custom("Cannot process 3".into())))
            } else {
                let result = controller.enqueue(chunk);
                future::ready(result)
            }
        }
    }

    #[localtest_macros::localset_test]
    async fn test_basic_transform() {
        let transformer = UppercaseTransformer;
        let transform_stream =
            TransformStream::builder(transformer).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_stream, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write some data
        writer.write("hello".to_string()).await.unwrap();
        writer.write("world".to_string()).await.unwrap();
        writer.close().await.unwrap();

        // Read the transformed data
        let result1 = timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result1, Some("HELLO".to_string()));

        let result2 = timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result2, Some("WORLD".to_string()));

        let result3 = timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result3, None); // Stream closed
    }

    #[localtest_macros::localset_test]
    async fn test_numeric_transform() {
        let transformer = DoubleTransformer;
        let transform_stream =
            TransformStream::builder(transformer).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write numbers
        writer.write(5).await.unwrap();
        writer.write(10).await.unwrap();
        writer.write(-3).await.unwrap();
        writer.close().await.unwrap();

        // Read doubled numbers
        assert_eq!(reader.read().await.unwrap(), Some(10));
        assert_eq!(reader.read().await.unwrap(), Some(20));
        assert_eq!(reader.read().await.unwrap(), Some(-6));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[localtest_macros::localset_test]
    async fn test_filtering_transform() {
        let transformer = OddFilterTransformer;
        let transform_stream =
            TransformStream::builder(transformer).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write mix of odd and even numbers
        writer.write(1).await.unwrap(); // odd - should pass
        writer.write(2).await.unwrap(); // even - should be filtered
        writer.write(3).await.unwrap(); // odd - should pass  
        writer.write(4).await.unwrap(); // even - should be filtered
        writer.write(5).await.unwrap(); // odd - should pass
        writer.close().await.unwrap();

        // Should only get odd numbers
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(3));
        assert_eq!(reader.read().await.unwrap(), Some(5));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[localtest_macros::localset_test]
    async fn test_transform_error_handling() {
        let transformer = ErrorOnThreeTransformer;
        let transform_stream =
            TransformStream::builder(transformer).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write data that will cause an error
        writer.write(1).await.unwrap();
        writer.write(2).await.unwrap();

        // This should work fine
        assert_eq!(reader.read().await.unwrap(), Some(1));
        assert_eq!(reader.read().await.unwrap(), Some(2));

        // This write should fail
        let write_result = writer.write(3).await;
        assert!(write_result.is_err());

        // Reading should now error too since transform errored
        let read_result = reader.read().await;
        assert!(read_result.is_err());
    }

    #[localtest_macros::localset_test]
    async fn test_empty_stream() {
        let transformer = UppercaseTransformer;
        let transform_stream =
            TransformStream::builder(transformer).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Close immediately without writing
        writer.close().await.unwrap();

        // Should get None immediately
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[localtest_macros::localset_test]
    async fn test_multiple_writes_before_read() {
        let transformer = DoubleTransformer;
        let transform_stream =
            TransformStream::builder(transformer).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write multiple items before reading any
        writer.write(1).await.unwrap();
        writer.write(2).await.unwrap();
        writer.write(3).await.unwrap();
        writer.close().await.unwrap();

        // All should be available to read
        assert_eq!(reader.read().await.unwrap(), Some(2));
        assert_eq!(reader.read().await.unwrap(), Some(4));
        assert_eq!(reader.read().await.unwrap(), Some(6));
        assert_eq!(reader.read().await.unwrap(), None);
    }

    #[localtest_macros::localset_test]
    async fn test_abort_stream() {
        let transformer = UppercaseTransformer;
        let transform_stream =
            TransformStream::builder(transformer).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        writer.write("hello".to_string()).await.unwrap();

        // Read one item
        assert_eq!(reader.read().await.unwrap(), Some("HELLO".to_string()));

        // Abort the writer
        let abort_result = writer.abort(Some("Test abort".to_string())).await;
        assert!(abort_result.is_err()); // Abort should return error

        // Subsequent reads should fail
        let read_result = reader.read().await;
        assert!(read_result.is_err());
    }

    #[localtest_macros::localset_test]
    async fn test_identity_transform_default() {
        let transform_stream =
            TransformStream::builder(IdentityTransformer::new()).spawn(tokio::task::spawn_local);
        let (readable, writable) = transform_stream.split();
        let (_stream, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        let numbers = vec![1, 2, 3, 4, 5];

        for &num in numbers.iter() {
            writer.write(num).await.unwrap();
        }
        writer.close().await.unwrap();

        for &num in numbers.iter() {
            let result = timeout(Duration::from_secs(1), reader.read())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result, Some(num));
        }

        let result_none = timeout(Duration::from_secs(1), reader.read())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result_none, None);
    }
}

#[cfg(test)]
mod usage_examples {
    use super::tests::*;
    use super::*;

    #[localtest_macros::localset_test]
    async fn test_three_spawners_approach() {
        let transformer = DoubleTransformer;

        let transform_stream = TransformStream::builder(transformer).spawn_parts(
            |fut| {
                tokio::task::spawn_local(fut);
            }, // readable
            |fut| {
                tokio::task::spawn_local(fut);
            }, // writable
            |fut| {
                tokio::task::spawn_local(fut);
            }, // transform
        );

        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write some numbers
        writer.write(5).await.unwrap();
        writer.write(10).await.unwrap();
        writer.write(-3).await.unwrap();
        writer.close().await.unwrap();

        // Verify they get doubled
        assert_eq!(reader.read().await.unwrap(), Some(10));
        assert_eq!(reader.read().await.unwrap(), Some(20));
        assert_eq!(reader.read().await.unwrap(), Some(-6));
        assert_eq!(reader.read().await.unwrap(), None);
    }
}
