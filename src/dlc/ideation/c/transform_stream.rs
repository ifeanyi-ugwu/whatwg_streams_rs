use super::stream_error::StreamError;

pub struct TransformStream<S, F, T>
where
    S: Stream<Item = Result<u32, StreamError>> + Unpin,
    F: Fn(u32) -> T,
{
    source: S,
    func: F,
}

impl<S, F, T> TransformStream<S, F, T>
where
    S: Stream<Item = Result<u32, StreamError>> + Unpin,
    F: Fn(u32) -> T,
{
    pub fn new(source: S, func: F) -> Self {
        Self { source, func }
    }
}

impl<S, F, T> Stream for TransformStream<S, F, T>
where
    S: Stream<Item = Result<u32, StreamError>> + Unpin,
    F: Fn(u32) -> T,
{
    type Item = Result<T, StreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.source).poll_next(cx) {
            Poll::Ready(Some(Ok(v))) => {
                let out = (this.func)(v);
                Poll::Ready(Some(Ok(out)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
