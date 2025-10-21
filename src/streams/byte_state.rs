use super::{
    byte_source_trait::ReadableByteSource, error::StreamError,
    readable::ReadableByteStreamController,
};
use crate::platform::{MaybeSend, MaybeSync, SharedPtr};
use futures::future::poll_fn;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

// Enhanced byte state that handles both buffering and source pulling
pub struct ByteStreamState<Source> {
    // Primary buffer for accumulated data
    buffer: Mutex<VecDeque<u8>>,

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
    /*pub(crate) pending_reads:
    Mutex<VecDeque<oneshot::Sender<Result<Option<Vec<u8>>, StreamError>>>>,*/
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
            //pending_reads: Mutex::new(VecDeque::new()),
            start_completed: AtomicBool::new(false),
            start_wakers: Mutex::new(Vec::new()),
        })
    }

    // Zero-copy read directly into caller's buffer
    pub fn poll_read_into(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, StreamError>> {
        // First, ensure start has completed
        if !self.start_completed.load(Ordering::SeqCst) {
            let mut wakers = self.start_wakers.lock();
            let waker = cx.waker();
            if !wakers.iter().any(|w| w.will_wake(waker)) {
                wakers.push(waker.clone());
            }
            return Poll::Pending;
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // Check error state first
        if self.errored.load(Ordering::SeqCst) {
            let error = self
                .error
                .lock()
                .clone()
                .unwrap_or_else(|| "Stream errored".into());
            return Poll::Ready(Err(error));
        }

        // Try to read from buffer
        let bytes_copied = {
            let mut buffer = self.buffer.lock();
            if !buffer.is_empty() {
                let bytes_to_read = std::cmp::min(buf.len(), buffer.len());

                // Zero-copy using VecDeque's as_slices for optimal performance
                let (first_slice, second_slice) = buffer.as_slices();
                let mut copied = 0;

                // Copy from first slice
                if !first_slice.is_empty() && copied < bytes_to_read {
                    let to_copy = std::cmp::min(first_slice.len(), bytes_to_read);
                    buf[..to_copy].copy_from_slice(&first_slice[..to_copy]);
                    copied += to_copy;
                }

                // Copy from second slice if needed
                if !second_slice.is_empty() && copied < bytes_to_read {
                    let to_copy = std::cmp::min(second_slice.len(), bytes_to_read - copied);
                    buf[copied..copied + to_copy].copy_from_slice(&second_slice[..to_copy]);
                    copied += to_copy;
                }

                // Remove copied bytes and update size tracking
                buffer.drain(..copied);
                let new_size = self
                    .queue_total_size
                    .load(Ordering::SeqCst)
                    .saturating_sub(copied);
                self.queue_total_size.store(new_size, Ordering::SeqCst);
                self.update_desired_size();

                copied
            } else {
                0
            }
        };

        if bytes_copied > 0 {
            // We got data, try to trigger a pull for next time if buffer is getting low
            self.maybe_trigger_pull();
            return Poll::Ready(Ok(bytes_copied));
        }

        // No data available, check if closed
        if self.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(0)); // EOF
        }

        // Register waker for when data becomes available
        {
            let mut wakers = self.read_wakers.lock();
            let waker = cx.waker();
            if !wakers.iter().any(|w| w.will_wake(waker)) {
                wakers.push(waker.clone());
            }
        }

        // Try to initiate a pull if one isn't already in progress
        self.maybe_trigger_pull();

        Poll::Pending
    }

    /*pub fn poll_read_chunk(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Vec<u8>>, StreamError>> {
        // Check error state first
        if self.errored.load(Ordering::SeqCst) {
            let error = self
                .error
                .lock()
                .clone()
                .unwrap_or_else(|| StreamError::Custom("Stream errored".into()));
            return Poll::Ready(Err(error));
        }

        // Try to get a chunk from the buffer
        let chunk = {
            let mut buffer = self.buffer.lock();
            if !buffer.is_empty() {
                // Determine a reasonable chunk size to return
                let chunk_size = std::cmp::min(1024, buffer.len());
                let mut chunk = Vec::with_capacity(chunk_size);

                // Drain from VecDeque and extend the new Vec
                chunk.extend(buffer.drain(..chunk_size));

                // Update the total size
                let new_size = self
                    .queue_total_size
                    .load(Ordering::SeqCst)
                    .saturating_sub(chunk.len());
                self.queue_total_size.store(new_size, Ordering::SeqCst);
                self.update_desired_size();

                Some(chunk)
            } else {
                None
            }
        };

        if let Some(data) = chunk {
            // We got data, so return it
            self.maybe_trigger_pull();
            return Poll::Ready(Ok(Some(data)));
        }

        // No data available, check if closed
        if self.closed.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(None)); // EOF
        }

        // No data available, register waker and wait for more data
        {
            let mut wakers = self.read_wakers.lock();
            let waker = cx.waker();
            if !wakers.iter().any(|w| w.will_wake(waker)) {
                wakers.push(waker.clone());
            }
        }

        // Try to initiate a pull if one isn't already in progress
        self.maybe_trigger_pull();

        Poll::Pending
    }*/

    // Internal method to trigger pulls when needed
    pub fn maybe_trigger_pull(&self) {
        // Only pull if:
        // 1. We're not already pulling
        // 2. We're not closed/errored
        // 3. Buffer is below high water mark
        let current_size = self.queue_total_size.load(Ordering::SeqCst);
        let hwm = self.high_water_mark.load(Ordering::SeqCst);

        if !self.pull_in_progress.load(Ordering::SeqCst)
            && !self.closed.load(Ordering::SeqCst)
            && !self.errored.load(Ordering::SeqCst)
            && current_size < hwm
        {
            self.needs_pull.store(true, Ordering::SeqCst);
            if let Some(waker) = self.pull_waker.lock().take() {
                waker.wake();
            }
        }
    }

    // Method for the stream task to poll for pull requests
    pub fn poll_pull_needed(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.needs_pull.load(Ordering::SeqCst) {
            self.needs_pull.store(false, Ordering::SeqCst);
            Poll::Ready(())
        } else {
            // Store waker for triggering pulls
            *self.pull_waker.lock() = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    // Called by source when data is produced
    pub fn enqueue_data(&self, data: &[u8]) {
        /*if self.byte_state.closed.load(Ordering::SeqCst) {
            return Err(StreamError::Custom("Stream is closed".into()));
        }
        if self.byte_state.errored.load(Ordering::SeqCst) {
            return Err(StreamError::Custom("Stream is errored".into()));
        }*/

        if data.is_empty() {
            return;
        }

        {
            let mut buffer = self.buffer.lock();
            buffer.extend(data.iter().cloned());
            let new_size = self.queue_total_size.load(Ordering::SeqCst) + data.len();
            self.queue_total_size.store(new_size, Ordering::SeqCst);
        }

        /*{
            let mut buffer = self.buffer.lock();
            let mut pending = self.pending_reads.lock();
            while !pending.is_empty() && !buffer.is_empty() {
                if let Some(tx) = pending.pop_front() {
                    let n = std::cmp::min(buffer.len(), 8192); // chunk size
                    let chunk: Vec<u8> = buffer.drain(..n).collect();
                    let _ = tx.send(Ok(Some(chunk)));
                }
            }
        }*/

        self.update_desired_size();
        self.wake_readers();
    }

    // Called when pull operation starts
    pub fn mark_pull_started(&self) {
        self.pull_in_progress.store(true, Ordering::SeqCst);
    }

    // Called when pull operation completes
    pub fn mark_pull_completed(&self) {
        self.pull_in_progress.store(false, Ordering::SeqCst);
        // Check if we need another pull
        self.maybe_trigger_pull();
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        /*{
            let mut pending = self.pending_reads.lock();
            while let Some(tx) = pending.pop_front() {
                let _ = tx.send(Ok(None)); // EOF
            }
        }*/
        self.update_desired_size();
        self.wake_readers();
        if let Some(waker) = self.pull_waker.lock().take() {
            waker.wake();
        }
    }

    pub fn error(&self, err: StreamError) {
        self.errored.store(true, Ordering::SeqCst);
        *self.error.lock() = Some(err);
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
        if self.closed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            self.desired_size.store(0, Ordering::SeqCst);
            return;
        }

        let hwm = self.high_water_mark.load(Ordering::SeqCst) as isize;
        let current = self.queue_total_size.load(Ordering::SeqCst) as isize;
        self.desired_size.store(hwm - current, Ordering::SeqCst);
    }

    pub fn desired_size(&self) -> Option<isize> {
        if self.closed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            None
        } else {
            Some(self.desired_size.load(Ordering::SeqCst))
        }
    }

    // Helper method to check if buffer is empty
    pub fn is_buffer_empty(&self) -> bool {
        self.buffer.lock().is_empty()
    }

    // Helper method to get current buffer size
    pub fn buffer_size(&self) -> usize {
        self.queue_total_size.load(Ordering::SeqCst)
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
        //self.start_completed.store(true, Ordering::SeqCst);
        if self.start_completed.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut wakers = self.start_wakers.lock();
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }
}

//#[async_trait]
pub trait ByteStreamStateInterface: MaybeSend + MaybeSync {
    fn desired_size(&self) -> Option<isize>;
    fn close(&self);
    fn enqueue_data(&self, data: &[u8]);
    fn error(&self, error: StreamError);
    fn is_buffer_empty(&self) -> bool;
    fn buffer_size(&self) -> usize;
    fn is_closed(&self) -> bool;
    fn is_errored(&self) -> bool;
    //async fn closed(&self) -> Result<(), StreamError>;
    //fn closed(&self) -> Pin<Box<dyn Future<Output = Result<(), StreamError>> + Send>>;
    fn closed(&self) -> crate::platform::PlatformBoxFuture<'_, Result<(), StreamError>>;
    fn poll_read_into(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, StreamError>>;
    fn cancel_source<'a>(
        &'a self,
        reason: Option<String>,
    ) -> crate::platform::PlatformBoxFuture<'a, Result<(), StreamError>>;
    //fn poll_read_chunk(&self, cx: &mut Context<'_>) -> Poll<Result<Option<Vec<u8>>, StreamError>>;
}

//#[async_trait]
impl<Source> ByteStreamStateInterface for ByteStreamState<Source>
where
    Source: ReadableByteSource + 'static,
{
    fn desired_size(&self) -> Option<isize> {
        //self.desired_size()
        ByteStreamState::desired_size(self)
    }

    fn close(&self) {
        //self.close();
        ByteStreamState::close(self)
    }

    fn enqueue_data(&self, data: &[u8]) {
        //self.enqueue_data(data);
        ByteStreamState::enqueue_data(self, data)
    }

    fn error(&self, error: StreamError) {
        //self.error(error);
        ByteStreamState::error(self, error)
    }

    fn is_buffer_empty(&self) -> bool {
        self.is_buffer_empty()
    }

    fn buffer_size(&self) -> usize {
        self.buffer_size()
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn is_errored(&self) -> bool {
        self.errored.load(Ordering::SeqCst)
    }

    /*async fn closed(&self) -> Result<(), StreamError> {
        ByteStreamState::closed(self).await
    }*/
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

    /*fn poll_read_chunk(&self, cx: &mut Context<'_>) -> Poll<Result<Option<Vec<u8>>, StreamError>> {
        ByteStreamState::poll_read_chunk(self, cx)
    }*/
}
