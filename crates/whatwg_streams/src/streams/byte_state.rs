use super::{
    byte_source_trait::ReadableByteSource, error::StreamError,
    readable::ReadableByteStreamController,
};
use crate::platform::{MaybeSend, MaybeSync, SharedPtr};
use bytes::{Buf, Bytes};
use futures::future::poll_fn;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

// Enhanced byte state that handles both buffering and source pulling
pub struct ByteStreamState<Source> {
    // Queue of immutable byte chunks. Bytes shares its backing allocation on
    // clone (refcount) and slices via offset math, so chunks move through the
    // queue and out to readers without copying their contents.
    buffer: Mutex<VecDeque<Bytes>>,

    pub(crate) source: Mutex<Option<Source>>,

    pub(crate) read_wakers: Mutex<Vec<Waker>>,
    pull_waker: Mutex<Option<Waker>>,

    pub(crate) closed: AtomicBool,
    pub(crate) errored: AtomicBool,
    pub(crate) error: Mutex<Option<StreamError>>,

    pub(crate) pull_in_progress: AtomicBool,
    needs_pull: AtomicBool,

    pub(crate) queue_total_size: AtomicUsize,
    pub(crate) high_water_mark: AtomicUsize,
    desired_size: AtomicIsize,
    start_completed: AtomicBool,
    start_wakers: Mutex<Vec<Waker>>,
}

impl<Source> ByteStreamState<Source>
where
    Source: ReadableByteSource + 'static,
{
    pub fn new(source: Source, high_water_mark: usize) -> SharedPtr<Self> {
        SharedPtr::new(Self {
            buffer: Mutex::new(VecDeque::new()),
            source: Mutex::new(Some(source)),
            read_wakers: Mutex::new(Vec::new()),
            pull_waker: Mutex::new(None),
            closed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: Mutex::new(None),
            pull_in_progress: AtomicBool::new(false),
            needs_pull: AtomicBool::new(false),
            queue_total_size: AtomicUsize::new(0),
            high_water_mark: AtomicUsize::new(high_water_mark),
            desired_size: AtomicIsize::new(high_water_mark as isize),
            start_completed: AtomicBool::new(false),
            start_wakers: Mutex::new(Vec::new()),
        })
    }

    // Registers `cx` and returns false while start() is still running, so reads
    // park until the source has had a chance to seed the queue.
    fn poll_start_ready(&self, cx: &mut Context<'_>) -> bool {
        if self.start_completed.load(Ordering::Acquire) {
            return true;
        }
        let mut wakers = self.start_wakers.lock();
        // Recheck under the lock to avoid racing mark_start_completed.
        if self.start_completed.load(Ordering::Acquire) {
            return true;
        }
        let waker = cx.waker();
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
        false
    }

    fn take_error(&self) -> StreamError {
        self.error
            .lock()
            .clone()
            .unwrap_or_else(|| "Stream errored".into())
    }

    fn register_read_waker(&self, cx: &mut Context<'_>) {
        let mut wakers = self.read_wakers.lock();
        let waker = cx.waker();
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }

    // BYOB read: fill the caller's buffer from the front of the queue, draining
    // across chunk boundaries. The destination is caller-owned, so this copies;
    // partial fills are allowed (returns however many bytes were available).
    pub fn poll_read_into(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, StreamError>> {
        if !self.poll_start_ready(cx) {
            return Poll::Pending;
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if self.errored.load(Ordering::Acquire) {
            return Poll::Ready(Err(self.take_error()));
        }

        let bytes_copied = {
            let mut buffer = self.buffer.lock();
            let mut copied = 0;
            while copied < buf.len() {
                let Some(front) = buffer.front_mut() else {
                    break;
                };
                let front_len = front.len();
                let n = std::cmp::min(buf.len() - copied, front_len);
                buf[copied..copied + n].copy_from_slice(&front[..n]);
                copied += n;
                if n == front_len {
                    buffer.pop_front();
                } else {
                    // Offset past the consumed prefix; the remainder stays at the
                    // front for the next read without moving any bytes.
                    front.advance(n);
                }
            }

            if copied > 0 {
                let new_size = self
                    .queue_total_size
                    .load(Ordering::Relaxed)
                    .saturating_sub(copied);
                self.queue_total_size.store(new_size, Ordering::Release);
                self.update_desired_size();
            }

            copied
        };

        if bytes_copied > 0 {
            self.maybe_trigger_pull();
            return Poll::Ready(Ok(bytes_copied));
        }

        if self.closed.load(Ordering::Acquire) {
            return Poll::Ready(Ok(0)); // EOF
        }

        self.register_read_waker(cx);
        self.maybe_trigger_pull();

        Poll::Pending
    }

    // Default read: hand the consumer one queue entry, whole, without copying its
    // contents (the spec dequeues exactly one entry per default read). Ok(None)
    // signals EOF.
    pub fn poll_read_chunk(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, StreamError>> {
        if !self.poll_start_ready(cx) {
            return Poll::Pending;
        }

        if self.errored.load(Ordering::Acquire) {
            return Poll::Ready(Err(self.take_error()));
        }

        let chunk = {
            let mut buffer = self.buffer.lock();
            match buffer.pop_front() {
                Some(chunk) => {
                    let new_size = self
                        .queue_total_size
                        .load(Ordering::Relaxed)
                        .saturating_sub(chunk.len());
                    self.queue_total_size.store(new_size, Ordering::Release);
                    self.update_desired_size();
                    Some(chunk)
                }
                None => None,
            }
        };

        if let Some(chunk) = chunk {
            self.maybe_trigger_pull();
            return Poll::Ready(Ok(Some(chunk)));
        }

        if self.closed.load(Ordering::Acquire) {
            return Poll::Ready(Ok(None)); // EOF
        }

        self.register_read_waker(cx);
        self.maybe_trigger_pull();

        Poll::Pending
    }

    // Internal method to trigger pulls when needed
    pub fn maybe_trigger_pull(&self) {
        // Only pull if:
        // 1. We're not already pulling
        // 2. We're not closed/errored
        // 3. Buffer is below high water mark
        let current_size = self.queue_total_size.load(Ordering::Acquire);
        let hwm = self.high_water_mark.load(Ordering::Acquire);

        if !self.pull_in_progress.load(Ordering::Acquire)
            && !self.closed.load(Ordering::Acquire)
            && !self.errored.load(Ordering::Acquire)
            && current_size < hwm
        {
            self.needs_pull.store(true, Ordering::Release);
            if let Some(waker) = self.pull_waker.lock().take() {
                waker.wake();
            }
        }
    }

    // Method for the stream task to poll for pull requests
    pub fn poll_pull_needed(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.needs_pull.load(Ordering::Acquire) {
            self.needs_pull.store(false, Ordering::Release);
            Poll::Ready(())
        } else {
            // Store waker for triggering pulls
            *self.pull_waker.lock() = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    // The producer transfers ownership of an already-allocated chunk into the
    // queue without copying its contents.
    pub fn enqueue_bytes(&self, chunk: Bytes) {
        if chunk.is_empty() {
            return;
        }

        let len = chunk.len();
        {
            let mut buffer = self.buffer.lock();
            buffer.push_back(chunk);
            let new_size = self.queue_total_size.load(Ordering::Relaxed) + len;
            self.queue_total_size.store(new_size, Ordering::Release);
        }

        self.update_desired_size();
        self.wake_readers();
    }

    // Called when pull operation starts
    pub fn mark_pull_started(&self) {
        self.pull_in_progress.store(true, Ordering::Release);
    }

    // Called when pull operation completes
    pub fn mark_pull_completed(&self) {
        self.pull_in_progress.store(false, Ordering::Release);
        // Check if we need another pull
        self.maybe_trigger_pull();
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.update_desired_size();
        self.wake_readers();
        if let Some(waker) = self.pull_waker.lock().take() {
            waker.wake();
        }
    }

    pub fn error(&self, err: StreamError) {
        *self.error.lock() = Some(err);
        self.errored.store(true, Ordering::Release);
        self.update_desired_size();
        self.wake_readers();
        if let Some(waker) = self.pull_waker.lock().take() {
            waker.wake();
        }
    }

    fn wake_readers(&self) {
        let mut wakers = self.read_wakers.lock();
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }

    fn update_desired_size(&self) {
        if self.closed.load(Ordering::Acquire) || self.errored.load(Ordering::Acquire) {
            self.desired_size.store(0, Ordering::Release);
            return;
        }

        let hwm = self.high_water_mark.load(Ordering::Relaxed) as isize;
        let current = self.queue_total_size.load(Ordering::Relaxed) as isize;
        self.desired_size.store(hwm - current, Ordering::Release);
    }

    pub fn desired_size(&self) -> Option<isize> {
        if self.closed.load(Ordering::Acquire) || self.errored.load(Ordering::Acquire) {
            None
        } else {
            Some(self.desired_size.load(Ordering::Acquire))
        }
    }

    // Helper method to check if buffer is empty
    pub fn is_buffer_empty(&self) -> bool {
        self.buffer.lock().is_empty()
    }

    // Helper method to get current buffer size
    pub fn buffer_size(&self) -> usize {
        self.queue_total_size.load(Ordering::Acquire)
    }

    pub async fn start_source(
        &self,
        controller: &ReadableByteStreamController,
    ) -> Result<(), StreamError> {
        let mut source = match self.source.lock().take() {
            Some(s) => s,
            None => return Ok(()),
        };

        let mut controller = controller.clone();

        let result = source.start(&mut controller).await;

        match result {
            Ok(()) => {
                *self.source.lock() = Some(source);
                self.mark_start_completed();
                Ok(())
            }
            Err(err) => {
                self.error(err.clone());
                self.mark_start_completed();
                Err(err)
            }
        }
    }

    pub async fn closed(&self) -> Result<(), StreamError> {
        poll_fn(|cx| {
            if self.is_errored() {
                let error = self
                    .error
                    .lock()
                    .clone()
                    .unwrap_or_else(|| "Stream errored".into());
                return Poll::Ready(Err(error));
            }

            if self.is_closed() {
                return Poll::Ready(Ok(()));
            }

            // Register waker so we wake once close() or error() is called
            let mut wakers = self.read_wakers.lock();
            let waker = cx.waker();
            if !wakers.iter().any(|w| w.will_wake(waker)) {
                wakers.push(waker.clone());
            }

            Poll::Pending
        })
        .await
    }

    pub fn mark_start_completed(&self) {
        if self.start_completed.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut wakers = self.start_wakers.lock();
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }
}

pub trait ByteStreamStateInterface: MaybeSend + MaybeSync {
    fn desired_size(&self) -> Option<isize>;
    fn close(&self);
    fn enqueue_bytes(&self, chunk: Bytes);
    fn error(&self, error: StreamError);
    fn is_buffer_empty(&self) -> bool;
    fn buffer_size(&self) -> usize;
    fn is_closed(&self) -> bool;
    fn is_errored(&self) -> bool;
    fn closed(&self) -> crate::platform::PlatformBoxFuture<'_, Result<(), StreamError>>;
    fn poll_read_into(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, StreamError>>;
    fn poll_read_chunk(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, StreamError>>;
    fn cancel_source<'a>(
        &'a self,
        reason: Option<String>,
    ) -> crate::platform::PlatformBoxFuture<'a, Result<(), StreamError>>;
}

impl<Source> ByteStreamStateInterface for ByteStreamState<Source>
where
    Source: ReadableByteSource + 'static,
{
    fn desired_size(&self) -> Option<isize> {
        ByteStreamState::desired_size(self)
    }

    fn close(&self) {
        ByteStreamState::close(self)
    }

    fn enqueue_bytes(&self, chunk: Bytes) {
        ByteStreamState::enqueue_bytes(self, chunk)
    }

    fn error(&self, error: StreamError) {
        ByteStreamState::error(self, error)
    }

    fn is_buffer_empty(&self) -> bool {
        self.is_buffer_empty()
    }

    fn buffer_size(&self) -> usize {
        self.buffer_size()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn is_errored(&self) -> bool {
        self.errored.load(Ordering::Acquire)
    }

    fn closed(&self) -> crate::platform::PlatformBoxFuture<'_, Result<(), StreamError>> {
        Box::pin(async move { ByteStreamState::closed(self).await })
    }

    fn poll_read_into(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, StreamError>> {
        ByteStreamState::poll_read_into(self, cx, buf)
    }

    fn poll_read_chunk(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, StreamError>> {
        ByteStreamState::poll_read_chunk(self, cx)
    }

    fn cancel_source<'a>(
        &'a self,
        reason: Option<String>,
    ) -> crate::platform::PlatformBoxFuture<'a, Result<(), StreamError>> {
        Box::pin(async move {
            // Take the source out under lock (synchronously)
            let source_opt = self.source.lock().take();

            if let Some(mut s) = source_opt {
                // Now no lock guard is held across .await
                s.cancel(reason).await
            } else {
                Ok(())
            }
        })
    }
}
