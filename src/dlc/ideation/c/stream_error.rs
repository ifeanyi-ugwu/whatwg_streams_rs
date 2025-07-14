use futures::{Sink, SinkExt, StreamExt, stream::Stream};
use std::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

// Error type
#[derive(Debug)]
pub enum StreamError {
    Canceled,
    TypeError(String),
    NetworkError(String),
    Custom(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canceled => write!(f, "canceled"),
            Self::TypeError(msg) => write!(f, "type error: {}", msg),
            Self::NetworkError(msg) => write!(f, "network error: {}", msg),
            Self::Custom(msg) => write!(f, "custom error: {}", msg),
        }
    }
}
