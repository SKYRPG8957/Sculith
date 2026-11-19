use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A one-way connection shutdown signal. Every broadcast checks it for each viewer.
#[derive(Default)]
pub(super) struct CloseSignal {
    closed: AtomicBool,
    waiters: Notify,
}

impl CloseSignal {
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Returns true to the caller that initiated shutdown.
    pub(super) fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.waiters.notify_waiters();
        true
    }

    pub(super) async fn wait(&self) {
        // Notified observes notify_waiters from its creation, even before its first poll.
        let notified = self.waiters.notified();
        if !self.is_closed() {
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CloseSignal;
    use std::task::{Context, Poll, Waker};

    #[test]
    fn close_wakes_existing_waiters_and_is_visible_to_late_waiters() {
        let signal = CloseSignal::default();
        let mut cx = Context::from_waker(Waker::noop());
        let mut first = std::pin::pin!(signal.wait());
        let mut second = std::pin::pin!(signal.wait());
        assert!(!signal.is_closed());
        assert_eq!(first.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(second.as_mut().poll(&mut cx), Poll::Pending);
        assert!(signal.close());
        assert!(signal.is_closed());
        assert!(!signal.close());
        assert_eq!(first.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(()));
        assert_eq!(std::pin::pin!(signal.wait()).poll(&mut cx), Poll::Ready(()));
    }
}
