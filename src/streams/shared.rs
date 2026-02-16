use crate::platform::SharedPtr;
use std::task::Waker;

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
