use crate::platform::SharedPtr;
use futures::future::poll_fn;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll, Waker};

pub type StreamResult<T> = Result<T, super::error::StreamError>;

/// A lightweight, thread-safe set storing multiple wakers.
/// It ensures wakers are stored without duplicates (based on `will_wake`).
#[derive(Clone, Default)]
pub struct WakerSet(SharedPtr<parking_lot::Mutex<Vec<Waker>>>);

impl WakerSet {
    /// Creates a new, empty `WakerSet`.
    pub fn new() -> Self {
        WakerSet(SharedPtr::new(parking_lot::Mutex::new(Vec::new())))
    }

    /// Adds a waker to the set.
    /// If a waker that would wake the same task is already present, it does not add a duplicate.
    pub fn register(&self, waker: &Waker) {
        let mut wakers = self.0.lock();
        if !wakers.iter().any(|w| w.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }

    /// Wake all registered wakers and clear the set.
    pub fn wake_all(&self) {
        let mut wakers = self.0.lock();
        for waker in wakers.drain(..) {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct AbortInner {
    aborted: AtomicBool,
    reason: parking_lot::Mutex<Option<String>>,
    wakers: WakerSet,
}

/// Read-only side of an abort, handed to whoever needs to react to it.
///
/// Pass one to [`pipe_to`] via [`StreamPipeOptions::signal`] to make the pipe
/// abortable. Unlike `futures::future::AbortRegistration`, the reason supplied
/// to [`AbortController::abort`] survives here: [`pipe_to`] reads it back with
/// [`reason()`] and forwards it into the source `cancel()` and destination
/// `abort()`, matching the spec's shutdown-with-action reason propagation.
///
/// Clones share one abort state, so a signal can be observed from several
/// places at once.
///
/// [`pipe_to`]: crate::streams::ReadableStream::pipe_to
/// [`StreamPipeOptions::signal`]: crate::streams::StreamPipeOptions::signal
/// [`reason()`]: AbortSignal::reason
#[derive(Clone, Default)]
pub struct AbortSignal {
    inner: SharedPtr<AbortInner>,
}

impl AbortSignal {
    /// Whether the signal has been aborted.
    pub fn aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::Acquire)
    }

    /// The reason passed to [`AbortController::abort`], if the signal has fired
    /// and a reason was given. `None` while not yet aborted, or when aborted
    /// without a reason.
    pub fn reason(&self) -> Option<String> {
        self.inner.reason.lock().clone()
    }

    /// A future that resolves once the signal is aborted (immediately if it
    /// already has been).
    pub fn aborted_future(&self) -> impl Future<Output = ()> + 'static {
        let inner = self.inner.clone();
        poll_fn(move |cx| {
            if inner.aborted.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            inner.wakers.register(cx.waker());
            // Re-check after registering: an abort between the first load and
            // registration would otherwise leave this waker un-woken.
            if inner.aborted.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    /// First abort wins; later calls are no-ops so the reason can't be
    /// overwritten once observers have acted on it.
    fn trigger(&self, reason: Option<String>) {
        // Hold the reason lock across the flag store so a later trigger can't
        // race in between, and so any reader taking the lock sees the final
        // reason paired with `aborted == true`.
        let mut guard = self.inner.reason.lock();
        if self.inner.aborted.load(Ordering::Acquire) {
            return;
        }
        *guard = reason;
        self.inner.aborted.store(true, Ordering::Release);
        drop(guard);
        self.inner.wakers.wake_all();
    }
}

/// Trigger side of an abort.
///
/// Hand the paired [`AbortSignal`] (from [`signal()`]) to an abortable
/// operation, then call [`abort()`] to stop it, optionally with a reason that
/// the operation can recover.
///
/// [`signal()`]: AbortController::signal
/// [`abort()`]: AbortController::abort
#[derive(Clone, Default)]
pub struct AbortController {
    signal: AbortSignal,
}

impl AbortController {
    pub fn new() -> Self {
        Self::default()
    }

    /// The signal to pass to the operation being controlled. Clones observe the
    /// same abort state.
    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// Abort the paired signal, optionally recording a reason. Idempotent: the
    /// first call's reason is the one observers see.
    pub fn abort(&self, reason: Option<String>) {
        self.signal.trigger(reason);
    }
}
