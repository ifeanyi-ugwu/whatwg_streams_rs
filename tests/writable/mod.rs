// WPT: streams/writable-streams/
// https://github.com/web-platform-tests/wpt/tree/master/streams/writable-streams
mod general;
mod close;
mod abort;
mod backpressure;
mod start;        // WPT: streams/writable-streams/start.any.js
mod write;        // WPT: streams/writable-streams/write.any.js
mod sink_trait;   // futures::Sink<T> surface (Rust-specific, no WPT equivalent)
mod async_write;  // futures::AsyncWrite surface (Rust-specific, no WPT equivalent)
