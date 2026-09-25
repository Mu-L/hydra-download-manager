// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded, coalescing event queue for asynchronous lifecycle and progress events.

use crate::abi::{hydra_event_callback, hydra_event_t, hydra_event_type_t as T};
use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Optional host callback container with user context pointer.
#[derive(Clone, Copy)]
struct Callback {
    f: hydra_event_callback,
    user_data: *mut std::ffi::c_void,
}

// SAFETY: `user_data` is an opaque pointer managed by the host application.
unsafe impl Send for Callback {}
// SAFETY: `user_data` is an opaque pointer managed by the host application.
unsafe impl Sync for Callback {}

struct Inner {
    /// Non-progress lifecycle events in arrival order.
    lifecycle: VecDeque<hydra_event_t>,
    /// Latest pending progress event per job.
    progress: HashMap<u64, hydra_event_t>,
    /// Job ID rotation queue for fair progress dispatch.
    rotation: VecDeque<u64>,
    /// Cumulative count of dropped low-priority events.
    dropped: u64,
    /// Closed flag set on engine shutdown.
    closed: bool,
    /// Bumped by `wake`, so a waiter can tell a wake from a spurious return
    /// and give up its wait rather than going back to sleep.
    wakes: u64,
    callback: Option<Callback>,
}

/// Thread-safe bounded event queue with progress coalescing.
pub(crate) struct EventQueue {
    inner: Mutex<Inner>,
    ready: Condvar,
    cap: usize,
}

/// Returns true if the event represents an un-droppable terminal lifecycle transition.
fn is_terminal(kind: T) -> bool {
    matches!(
        kind,
        T::HYDRA_EVENT_COMPLETED
            | T::HYDRA_EVENT_FAILED
            | T::HYDRA_EVENT_CANCELLED
            | T::HYDRA_EVENT_ENGINE_SHUTDOWN
    )
}

/// Events after which the job publishes nothing more until it is started again,
/// so a progress sample still pending from before must not be delivered after.
fn ends_the_attempt(kind: T) -> bool {
    is_terminal(kind) || kind == T::HYDRA_EVENT_PAUSED
}

impl EventQueue {
    /// Creates a new event queue with the specified capacity limit for lifecycle events.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                lifecycle: VecDeque::new(),
                progress: HashMap::new(),
                rotation: VecDeque::new(),
                dropped: 0,
                closed: false,
                wakes: 0,
                callback: None,
            }),
            ready: Condvar::new(),
            cap: cap.max(8),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Publishes an event to the queue and notifies waiters/callbacks.
    pub(crate) fn push(&self, mut ev: hydra_event_t) {
        let cb = {
            let mut g = self.lock();
            if g.closed {
                return;
            }
            if ev.kind == T::HYDRA_EVENT_PROGRESS {
                if g.progress.insert(ev.job_id, ev).is_none() {
                    g.rotation.push_back(ev.job_id);
                }
            } else {
                if ends_the_attempt(ev.kind) {
                    g.progress.remove(&ev.job_id);
                }
                if g.lifecycle.len() >= self.cap {
                    if let Some(i) = g.lifecycle.iter().position(|e| !is_terminal(e.kind)) {
                        g.lifecycle.remove(i);
                        g.dropped += 1;
                    }
                }
                let dropped = g.dropped;
                ev.dropped_events = dropped;
                g.lifecycle.push_back(ev);
            }
            g.callback
        };
        self.ready.notify_one();
        if let Some(c) = cb {
            if let Some(f) = c.f {
                // SAFETY: callback pointer is supplied by host via `hydra_event_set_callback`.
                unsafe { f(&ev as *const hydra_event_t, c.user_data) };
            }
        }
    }

    /// Returns the next pending event without blocking, if available.
    pub(crate) fn try_next(&self) -> Option<hydra_event_t> {
        let mut g = self.lock();
        Self::pop(&mut g)
    }

    fn pop(g: &mut Inner) -> Option<hydra_event_t> {
        if let Some(e) = g.lifecycle.pop_front() {
            return Some(e);
        }
        let dropped = g.dropped;
        while let Some(id) = g.rotation.pop_front() {
            if let Some(mut e) = g.progress.remove(&id) {
                e.dropped_events = dropped;
                return Some(e);
            }
        }
        None
    }

    /// Wait up to `timeout` for an event.
    ///
    /// Returns `None` on timeout, on a `wake`, or once the queue is closed. A
    /// closed queue returns immediately rather than making a consumer thread
    /// wait out its timeout during shutdown.
    pub(crate) fn wait(&self, timeout: Option<Duration>) -> Option<hydra_event_t> {
        let mut g = self.lock();
        if let Some(e) = Self::pop(&mut g) {
            return Some(e);
        }
        let entered = g.wakes;
        match timeout {
            None => loop {
                if g.closed || g.wakes != entered {
                    return None;
                }
                g = self.ready.wait(g).unwrap_or_else(|p| p.into_inner());
                if let Some(e) = Self::pop(&mut g) {
                    return Some(e);
                }
            },
            Some(t) => {
                let deadline = Instant::now() + t;
                loop {
                    if g.closed || g.wakes != entered {
                        return None;
                    }
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    let (ng, _) = self
                        .ready
                        .wait_timeout(g, left)
                        .unwrap_or_else(|p| p.into_inner());
                    g = ng;
                    if let Some(e) = Self::pop(&mut g) {
                        return Some(e);
                    }
                }
            }
        }
    }

    /// Install or clear the optional convenience callback.
    pub(crate) fn set_callback(&self, f: hydra_event_callback, user_data: *mut std::ffi::c_void) {
        let mut g = self.lock();
        g.callback = f.map(|_| Callback { f, user_data });
    }

    /// Release every waiter without closing the queue.
    ///
    /// This is what lets a consumer thread blocked in `hydra_event_wait` be
    /// told to look at something else — a shutdown flag of the host's own —
    /// without the engine having to shut down first.
    pub(crate) fn wake(&self) {
        self.lock().wakes += 1;
        self.ready.notify_all();
    }

    /// Stop accepting events and release every waiter.
    pub(crate) fn close(&self) {
        {
            let mut g = self.lock();
            g.closed = true;
        }
        self.ready.notify_all();
    }

    /// How many low-priority events have been discarded.
    pub(crate) fn dropped(&self) -> u64 {
        self.lock().dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::hydra_event_type_t as T;
    use std::sync::Arc;

    fn ev(kind: T, job: u64) -> hydra_event_t {
        hydra_event_t {
            kind,
            job_id: job,
            ..Default::default()
        }
    }

    #[test]
    fn progress_events_coalesce_per_job() {
        let q = EventQueue::new(64);
        for i in 0..500u64 {
            let mut e = ev(T::HYDRA_EVENT_PROGRESS, 7);
            e.progress.bytes_downloaded = i;
            q.push(e);
        }
        let got = q.try_next().expect("one pending progress event");
        assert_eq!(got.progress.bytes_downloaded, 499, "newest sample survives");
        assert!(q.try_next().is_none(), "and it was the only one");
    }

    #[test]
    fn progress_rotation_is_fair_across_jobs() {
        let q = EventQueue::new(64);
        for job in [1u64, 2, 3] {
            q.push(ev(T::HYDRA_EVENT_PROGRESS, job));
        }
        q.push(ev(T::HYDRA_EVENT_PROGRESS, 1));
        let ids: Vec<u64> = (0..3)
            .filter_map(|_| q.try_next())
            .map(|e| e.job_id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn terminal_events_are_never_dropped() {
        let q = EventQueue::new(8);
        for _ in 0..8 {
            q.push(ev(T::HYDRA_EVENT_COMPLETED, 1));
        }
        // Far past the bound, and all of them terminal: the queue grows rather
        // than losing a completion.
        for _ in 0..64 {
            q.push(ev(T::HYDRA_EVENT_COMPLETED, 1));
        }
        let n = std::iter::from_fn(|| q.try_next()).count();
        assert_eq!(n, 72);
    }

    #[test]
    fn lifecycle_events_drop_oldest_and_are_counted() {
        let q = EventQueue::new(8);
        for _ in 0..8 {
            q.push(ev(T::HYDRA_EVENT_RETRYING, 1));
        }
        q.push(ev(T::HYDRA_EVENT_COMPLETED, 1));
        assert_eq!(q.dropped(), 1);
        let kinds: Vec<T> = std::iter::from_fn(|| q.try_next())
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds.len(), 8);
        assert_eq!(*kinds.last().unwrap(), T::HYDRA_EVENT_COMPLETED);
    }

    #[test]
    fn lifecycle_events_precede_pending_progress() {
        let q = EventQueue::new(8);
        q.push(ev(T::HYDRA_EVENT_PROGRESS, 1));
        q.push(ev(T::HYDRA_EVENT_COMPLETED, 1));
        assert_eq!(
            q.try_next().unwrap().kind,
            T::HYDRA_EVENT_COMPLETED,
            "a completion must not wait behind a progress sample"
        );
    }

    #[test]
    fn an_attempt_ending_event_discards_the_pending_progress_sample() {
        for ending in [
            T::HYDRA_EVENT_COMPLETED,
            T::HYDRA_EVENT_FAILED,
            T::HYDRA_EVENT_CANCELLED,
            T::HYDRA_EVENT_PAUSED,
        ] {
            let q = EventQueue::new(8);
            q.push(ev(T::HYDRA_EVENT_PROGRESS, 1));
            q.push(ev(T::HYDRA_EVENT_PROGRESS, 2));
            q.push(ev(ending, 1));
            let kinds: Vec<(T, u64)> = std::iter::from_fn(|| q.try_next())
                .map(|e| (e.kind, e.job_id))
                .collect();
            assert_eq!(
                kinds,
                vec![(ending, 1), (T::HYDRA_EVENT_PROGRESS, 2)],
                "{ending:?} must be job 1's last event, and job 2 is untouched"
            );
        }
    }

    #[test]
    fn wake_releases_a_waiter_without_an_event() {
        let q = Arc::new(EventQueue::new(8));
        let waiter = {
            let q = q.clone();
            std::thread::spawn(move || q.wait(None))
        };
        // Until the waiter is parked a wake is a no-op for it, and there is no
        // way to observe parking from outside: keep waking until it returns.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !waiter.is_finished() {
            assert!(Instant::now() < deadline, "the waiter never woke");
            q.wake();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(waiter.join().unwrap().is_none(), "a wake delivers no event");
        assert!(q.try_next().is_none(), "and consumes nothing");
    }

    #[test]
    fn wait_returns_immediately_once_closed() {
        let q = EventQueue::new(8);
        q.close();
        assert!(q.wait(Some(Duration::from_secs(30))).is_none());
        q.push(ev(T::HYDRA_EVENT_PROGRESS, 1));
        assert!(q.try_next().is_none(), "a closed queue accepts nothing");
    }
}
