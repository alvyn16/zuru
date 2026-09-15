use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Condvar, Mutex,
};

/// One queued request, with cooperative cancellation of work already in flight.
pub struct LatestSender<T>(Arc<Mailbox<T>>);
pub struct LatestReceiver<T>(Arc<Mailbox<T>>);
struct Mailbox<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
    revision: Arc<AtomicU64>,
}
struct State<T> {
    item: Option<(u64, T)>,
    closed: bool,
}

#[derive(Clone)]
pub struct CancellationToken {
    revision: Arc<AtomicU64>,
    expected: u64,
}
impl CancellationToken {
    pub fn is_cancelled(&self) -> bool {
        self.revision.load(Ordering::Acquire) != self.expected
    }
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.is_cancelled(), "Preview superseded");
        Ok(())
    }
}

pub fn latest_channel<T>() -> (LatestSender<T>, LatestReceiver<T>) {
    let inner = Arc::new(Mailbox {
        state: Mutex::new(State {
            item: None,
            closed: false,
        }),
        ready: Condvar::new(),
        revision: Arc::new(AtomicU64::new(0)),
    });
    (LatestSender(inner.clone()), LatestReceiver(inner))
}
impl<T> LatestSender<T> {
    pub fn send(&self, item: T) {
        let mut state = self.0.state.lock().expect("worker mailbox poisoned");
        let revision = self.0.revision.fetch_add(1, Ordering::AcqRel) + 1;
        state.item = Some((revision, item));
        self.0.ready.notify_one();
    }
    /// Invalidate queued and in-flight work when selection becomes empty.
    pub fn cancel(&self) {
        let mut state = self.0.state.lock().expect("worker mailbox poisoned");
        self.0.revision.fetch_add(1, Ordering::AcqRel);
        state.item = None;
    }
}
impl<T> Drop for LatestSender<T> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("worker mailbox poisoned");
        state.closed = true;
        self.0.revision.fetch_add(1, Ordering::AcqRel);
        self.0.ready.notify_one();
    }
}
impl<T> LatestReceiver<T> {
    pub fn recv(&self) -> Option<T> {
        self.recv_cancellable().map(|(item, _)| item)
    }
    pub fn recv_cancellable(&self) -> Option<(T, CancellationToken)> {
        let mut state = self.0.state.lock().expect("worker mailbox poisoned");
        while state.item.is_none() && !state.closed {
            state = self.0.ready.wait(state).expect("worker mailbox poisoned");
        }
        if state.closed {
            None
        } else {
            state.item.take().map(|(expected, item)| {
                (
                    item,
                    CancellationToken {
                        revision: self.0.revision.clone(),
                        expected,
                    },
                )
            })
        }
    }
}
