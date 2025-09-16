use super::super::{CountQueuingStrategy, QueuingStrategy, Unlocked, errors::StreamError};
use super::{
    readable::{DefaultStream, ReadableSource, ReadableStream, ReadableStreamDefaultController},
    writable::{WritableSink, WritableStream, WritableStreamDefaultController},
};
use futures::{FutureExt, pin_mut};
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
    /// Create a new TransformStream with default strategies
    pub fn new_with_spawn<T, F>(transformer: T, spawn_fn: F) -> Self
    where
        T: Transformer<I, O> + 'static,
        F: FnOnce(futures::future::LocalBoxFuture<'static, ()>),
    {
        Self::new_with_strategies_and_spawn(
            transformer,
            CountQueuingStrategy::new(1),
            CountQueuingStrategy::new(1),
            spawn_fn,
        )
    }

    /// Create a new TransformStream with custom strategies
    pub fn new_with_strategies_and_spawn<T, WS, RS, F>(
        transformer: T,
        writable_strategy: WS,
        readable_strategy: RS,
        spawn_fn: F,
    ) -> Self
    where
        T: Transformer<I, O> + 'static,
        WS: QueuingStrategy<I> + 'static,
        RS: QueuingStrategy<O> + 'static,
        F: FnOnce(futures::future::LocalBoxFuture<'static, ()>),
    {
        let (transform_tx, transform_rx) = unbounded::<TransformCommand<I>>();

        // Create the readable source (minimal, just needs to exist)
        let readable_source = TransformReadableSource::new();

        // Create the writable sink that sends data to be transformed
        let writable_sink = TransformWritableSink::new(transform_tx);

        // Create the streams - they handle their own queuing
        //let readable = ReadableStream::new_with_strategy(readable_source, readable_strategy);
        /*let writable =
        WritableStream::new_with_spawn(writable_sink, Box::new(writable_strategy), |fut| {
            spawn_fn(fut)
        });*/
        let (readable, mut readable_pool) = ReadableStream::new_with_pool_and_strategy(
            readable_source,
            CountQueuingStrategy::new(1),
        );
        let (writable, mut writable_pool) =
            WritableStream::new_with_pool(writable_sink, Box::new(CountQueuingStrategy::new(1)));

        // Spawn the transform task
        let readable_controller = readable.controller.clone();
        let writable_controller = writable.controller.clone();
        let controller =
            TransformStreamDefaultController::new(readable_controller, writable_controller);

        /*spawn_fn(Box::pin(transform_task(
            transformer,
            transform_rx,
            controller,
        )));*/

        // Spawn a single task that drives all three components
        /*spawn_fn(Box::pin(async move {
            let transform_fut = transform_task(transformer, transform_rx, controller);
            pin_mut!(transform_fut);

            loop {
                futures::select! {
                    result = &mut transform_fut => {
                        // Optional cleanup
                        readable_pool.run_until_stalled();
                        writable_pool.run_until_stalled();
                        break;
                    }
                    _ = async { readable_pool.run_until_stalled(); } => {}
                    _ = async { writable_pool.run_until_stalled(); } => {}
                }
            }
        }));*/

        /*spawn_fn(Box::pin(async move {
            let transform_fut = transform_task(transformer, transform_rx, controller).fuse();
            pin_mut!(transform_fut);

            loop {
                let readable_work = async { readable_pool.run_until_stalled() }.fuse();
                let writable_work = async { writable_pool.run_until_stalled() }.fuse();
                pin_mut!(readable_work, writable_work);

                futures::select! {
                    result = &mut transform_fut => {
                        // Optional cleanup
                        readable_pool.run_until_stalled();
                        writable_pool.run_until_stalled();
                        break;
                    }
                    _ = &mut readable_work => {}
                    _ = &mut writable_work => {}
                }
            }
        }));*/

        spawn_fn(Box::pin(async move {
            let transform_fut = transform_task(transformer, transform_rx, controller).fuse();
            pin_mut!(transform_fut);

            loop {
                futures::select! {
                    result = &mut transform_fut => {
                        // Optional cleanup
                        readable_pool.run_until_stalled();
                        writable_pool.run_until_stalled();
                        break;
                    }
                    _ = async { readable_pool.run_until_stalled() }.fuse() => {}
                    _ = async { writable_pool.run_until_stalled() }.fuse() => {}
                }
            }
        }));

        TransformStream { readable, writable }
    }

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

/*impl<I: 'static, O: 'static> TransformStream<I, O> {
    pub fn new_with_spawn_fn<T, F>(transformer: T, spawn_fn: F) -> Self
    where
        T: Transformer<I, O> + 'static,
        F: Fn(futures::future::LocalBoxFuture<'static, ()>),
    {
        let readable = ReadableStream::new_with_reusable_spawn(source, strategy, &spawn_fn);
        let writable = WritableStream::new_with_reusable_spawn(sink, strategy, &spawn_fn);
        spawn_fn(transform_task(...));
    }
}*/

impl<I: 'static, O: 'static> TransformStream<I, O> {
    pub fn new_with_three_spawners<T, F1, F2, F3>(
        transformer: T,
        readable_spawn: F1,
        writable_spawn: F2,
        transform_spawn: F3,
    ) -> Self
    where
        T: Transformer<I, O> + 'static,
        F1: FnOnce(futures::future::LocalBoxFuture<'static, ()>),
        F2: FnOnce(futures::future::LocalBoxFuture<'static, ()>),
        F3: FnOnce(futures::future::LocalBoxFuture<'static, ()>),
    {
        let (transform_tx, transform_rx) = unbounded::<TransformCommand<I>>();

        let readable_source = TransformReadableSource::new();
        let writable_sink = TransformWritableSink::new(transform_tx);

        // Each gets its own dedicated spawner
        let readable = ReadableStream::new_with_spawn(
            readable_source,
            //CountQueuingStrategy::new(1),
            readable_spawn,
        );

        let writable = WritableStream::new_with_spawn(
            writable_sink,
            Box::new(CountQueuingStrategy::new(1)),
            writable_spawn,
        );

        let readable_controller = readable.controller.clone();
        let writable_controller = writable.controller.clone();
        let controller =
            TransformStreamDefaultController::new(readable_controller, writable_controller);

        transform_spawn(Box::pin(transform_task(
            transformer,
            transform_rx,
            controller,
        )));

        TransformStream { readable, writable }
    }
}

impl<I: 'static, O: 'static> TransformStream<I, O> {
    pub fn new_with_spawner_factory<T, SF, F>(transformer: T, spawner_factory: SF) -> Self
    where
        T: Transformer<I, O> + 'static,
        SF: Fn() -> F,
        F: FnOnce(futures::future::LocalBoxFuture<'static, ()>),
    {
        let (transform_tx, transform_rx) = unbounded::<TransformCommand<I>>();

        let readable_source = TransformReadableSource::new();
        let writable_sink = TransformWritableSink::new(transform_tx);

        // Create a new spawner for each component
        let readable = ReadableStream::new_with_spawn(
            readable_source,
            // CountQueuingStrategy::new(1),
            spawner_factory(), // Create new spawner
        );

        let writable = WritableStream::new_with_spawn(
            writable_sink,
            Box::new(CountQueuingStrategy::new(1)),
            spawner_factory(), // Create new spawner
        );

        let readable_controller = readable.controller.clone();
        let writable_controller = writable.controller.clone();
        let controller =
            TransformStreamDefaultController::new(readable_controller, writable_controller);

        spawner_factory()(Box::pin(transform_task(
            // Create new spawner
            transformer,
            transform_rx,
            controller,
        )));

        TransformStream { readable, writable }
    }
}

impl<I: 'static, O: 'static> TransformStream<I, O> {
    pub fn new_with_rc_spawner<T>(
        transformer: T,
        spawner: std::rc::Rc<dyn Fn(futures::future::LocalBoxFuture<'static, ()>)>,
    ) -> Self
    where
        T: Transformer<I, O> + 'static,
    {
        let (transform_tx, transform_rx) = unbounded::<TransformCommand<I>>();

        let readable_source = TransformReadableSource::new();
        let writable_sink = TransformWritableSink::new(transform_tx);

        // Clone the Rc for each use
        let readable_spawner = spawner.clone();
        let writable_spawner = spawner.clone();
        let transform_spawner = spawner.clone();

        // But this still won't work because new_with_spawn expects FnOnce, not Fn
        // You'd need to wrap each call:
        let readable = ReadableStream::new_with_spawn(
            readable_source,
            //CountQueuingStrategy::new(1),
            move |fut| readable_spawner(fut),
        );

        let writable = WritableStream::new_with_spawn(
            writable_sink,
            Box::new(CountQueuingStrategy::new(1)),
            move |fut| writable_spawner(fut),
        );

        let readable_controller = readable.controller.clone();
        let writable_controller = writable.controller.clone();
        let controller =
            TransformStreamDefaultController::new(readable_controller, writable_controller);

        transform_spawner(Box::pin(transform_task(
            transformer,
            transform_rx,
            controller,
        )));

        TransformStream { readable, writable }
    }
}

/*/// Creates a new `TransformStream` that acts as an **identity transform**,
/// passing chunks from the writable side directly to the readable side
/// without modification.
///
/// This provides a convenient way to create a pair of connected
/// readable and writable streams for tasks like piping or buffering.
///
/// # Panics
/// This method does not panic.
///
/// # Example
/// ```rust
/// use crate::transform::TransformStream;
///
/// let transform_stream = TransformStream::<String, String>::default();
/// let (readable, writable) = transform_stream.split();
/// ```
impl<T: 'static> Default for TransformStream<T, T> {
    fn default() -> Self {
        Self::new(IdentityTransformer::new())
    }
}*/

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

            TransformCommand::Abort {
                reason: _,
                completion,
            } => {
                let error = StreamError::Custom("Transform stream aborted".into());
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
        let transform_stream = TransformStream::new_with_spawn(transformer, |fut| {
            tokio::task::spawn_local(fut);
        });
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
        let transform_stream = TransformStream::new_with_spawn(transformer, |fut| {
            tokio::task::spawn_local(fut);
        });
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
        let transform_stream = TransformStream::new_with_spawn(transformer, |fut| {
            tokio::task::spawn_local(fut);
        });
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
        let transform_stream = TransformStream::new_with_spawn(transformer, |fut| {
            tokio::task::spawn_local(fut);
        });
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
        let transform_stream = TransformStream::new_with_spawn(transformer, |fut| {
            tokio::task::spawn_local(fut);
        });
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
        let transform_stream = TransformStream::new_with_spawn(transformer, |fut| {
            tokio::task::spawn_local(fut);
        });
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
        let transform_stream = TransformStream::new_with_spawn(transformer, |fut| {
            tokio::task::spawn_local(fut);
        });
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
            TransformStream::<i32, i32>::new_with_spawn(IdentityTransformer::new(), |fut| {
                tokio::task::spawn_local(fut);
            });
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
    use std::time::Duration;
    use tokio::time::timeout;

    #[localtest_macros::localset_test]
    async fn test_three_spawners_approach() {
        let transformer = DoubleTransformer;

        let transform_stream = TransformStream::new_with_three_spawners(
            transformer,
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

    #[localtest_macros::localset_test]
    async fn test_spawner_factory_approach() {
        let transformer = UppercaseTransformer;

        let transform_stream = TransformStream::new_with_spawner_factory(
            transformer,
            || {
                |fut| {
                    tokio::task::spawn_local(fut);
                }
            }, // Factory returns new spawner each time
        );

        let (readable, writable) = transform_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Write some strings
        writer.write("hello".to_string()).await.unwrap();
        writer.write("world".to_string()).await.unwrap();
        writer.close().await.unwrap();

        // Verify they get uppercased
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
        assert_eq!(result3, None);
    }

    #[localtest_macros::localset_test]
    async fn test_rc_spawner_approach() {
        let transformer = OddFilterTransformer;

        let spawner = std::rc::Rc::new(|fut| {
            tokio::task::spawn_local(fut);
        });

        let transform_stream = TransformStream::new_with_rc_spawner(transformer, spawner);

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
}
