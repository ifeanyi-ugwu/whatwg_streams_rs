//! Shared sink and source helpers used across the WPT integration suite.

use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};

// ── CollectSink ───────────────────────────────────────────────────────────────

/// Sink that records every write, plus whether it was closed or aborted.
/// Use `Arc::default()` for fields you don't need to inspect.
#[cfg(feature = "send")]
pub struct CollectSink<T> {
    pub collected: std::sync::Arc<std::sync::Mutex<Vec<T>>>,
    pub closed: std::sync::Arc<std::sync::Mutex<bool>>,
    pub aborted: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(feature = "send")]
impl<T: Send + 'static> WritableSink<T> for CollectSink<T> {
    async fn write(
        &mut self,
        chunk: T,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.collected.lock().unwrap().push(chunk);
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        *self.closed.lock().unwrap() = true;
        Ok(())
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        *self.aborted.lock().unwrap() = reason;
        Ok(())
    }
}

// ── LifecycleSink ─────────────────────────────────────────────────────────────

/// Sink that tracks the full write/close/abort lifecycle.
#[cfg(feature = "send")]
pub struct LifecycleSink {
    pub writes: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    pub closed: std::sync::Arc<std::sync::Mutex<bool>>,
    pub aborted: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(feature = "send")]
impl Default for LifecycleSink {
    fn default() -> Self {
        Self {
            writes: Default::default(),
            closed: Default::default(),
            aborted: Default::default(),
        }
    }
}

#[cfg(feature = "send")]
impl WritableSink<u32> for LifecycleSink {
    async fn write(
        &mut self,
        chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.writes.lock().unwrap().push(chunk);
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        *self.closed.lock().unwrap() = true;
        Ok(())
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        *self.aborted.lock().unwrap() = reason;
        Ok(())
    }
}

// ── SlowSink ──────────────────────────────────────────────────────────────────

/// Sink whose write() blocks until the caller fires the `unblock` notify.
#[cfg(feature = "send")]
pub struct SlowSink {
    pub unblock: std::sync::Arc<tokio::sync::Notify>,
    pub write_count: std::sync::Arc<std::sync::Mutex<usize>>,
}

#[cfg(feature = "send")]
impl WritableSink<u32> for SlowSink {
    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        *self.write_count.lock().unwrap() += 1;
        self.unblock.notified().await;
        Ok(())
    }
}

// ── FailAfterSink ─────────────────────────────────────────────────────────────

/// Sink that succeeds for the first `fail_after` writes, then rejects.
#[cfg(feature = "send")]
pub struct FailAfterSink {
    pub fail_after: usize,
    pub count: std::sync::Arc<std::sync::Mutex<usize>>,
}

#[cfg(feature = "send")]
impl WritableSink<u32> for FailAfterSink {
    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        if *c > self.fail_after {
            Err("sink write failed".into())
        } else {
            Ok(())
        }
    }
}
