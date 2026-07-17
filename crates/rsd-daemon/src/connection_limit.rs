//! Small non-blocking admission limit for connection-per-thread surfaces.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub(crate) struct ConnectionLimit {
    active: AtomicUsize,
    max: usize,
}

impl ConnectionLimit {
    pub(crate) fn new(max: usize) -> Arc<Self> {
        assert!(max > 0);
        Arc::new(Self {
            active: AtomicUsize::new(0),
            max,
        })
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.max).then_some(active + 1)
            })
            .ok()
            .map(|_| ConnectionPermit(self.clone()))
    }
}

pub(crate) struct ConnectionPermit(Arc<ConnectionLimit>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_are_bounded_and_returned_on_drop() {
        let limit = ConnectionLimit::new(2);
        let first = limit.try_acquire().unwrap();
        let second = limit.try_acquire().unwrap();
        assert!(limit.try_acquire().is_none());
        drop(first);
        assert!(limit.try_acquire().is_some());
        drop(second);
    }
}
