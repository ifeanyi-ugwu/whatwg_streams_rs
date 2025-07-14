use std::{
    pin::Pin,
    task::{Context, Poll},
};

use tokio::sync::mpsc;

use super::stream_error::StreamError;

pub struct ChannelSink<T> {
    sender: mpsc::Sender<T>,
}

impl<T> ChannelSink<T> {
    pub fn new(sender: mpsc::Sender<T>) -> Self {
        Self { sender }
    }
}

impl<T> Sink<T> for ChannelSink<T> {
    type Error = StreamError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.get_mut();
        this.sender
            .try_send(item)
            .map_err(|e| StreamError::Custom(e.to_string()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}
