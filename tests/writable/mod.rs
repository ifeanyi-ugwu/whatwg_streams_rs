// WPT: streams/writable-streams/
// https://github.com/web-platform-tests/wpt/tree/master/streams/writable-streams
mod general;
mod close;
mod abort;
mod backpressure;
mod sink_trait;   // futures::Sink<T> surface (Rust-specific, no WPT equivalent)
mod async_write;  // futures::AsyncWrite surface (Rust-specific, no WPT equivalent)
