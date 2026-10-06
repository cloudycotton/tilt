//! FrameHub: the latest captured frame, viewer demand and new-frame notification
//! (brief section 5.2).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel::{Receiver, Sender};

use crate::session::SessionId;
use crate::video::convert::PooledFrame;

/// One captured screen converted to I420, shared with the encoders as `Arc<Frame>`.
pub struct Frame {
    /// Capture counter: a different seq means a newer image.
    pub seq: u64,
    /// clock::now_us() at the grab.
    pub capture_us: u64,
    /// Even-cropped screen size.
    pub image: PooledFrame,
}

pub struct FrameHub {
    inner: Mutex<Inner>,
    wake: Sender<()>,
}

struct Inner {
    latest: Option<Arc<Frame>>,
    /// The screen has changed since `latest` was grabbed (see `screen_changed`).
    stale: bool,
    subscribers: HashMap<SessionId, Subscriber>,
    screen: (u32, u32),
}

struct Subscriber {
    /// Wakes the session's worker (a bounded(1) channel: one pending wake-up covers all).
    wake: Sender<()>,
    wants_new: bool,
}

impl FrameHub {
    /// The hub, and the receiver of its wake-ups, which belongs to the capture thread.
    pub fn new() -> (Arc<FrameHub>, Receiver<()>) {
        // Capacity 1: a pending wake-up already covers any later ones.
        let (wake, wake_rx) = crossbeam_channel::bounded(1);
        let inner = Inner {
            latest: None,
            stale: false,
            subscribers: HashMap::new(),
            screen: (0, 0),
        };
        (
            Arc::new(FrameHub {
                inner: Mutex::new(inner),
                wake,
            }),
            wake_rx,
        )
    }

    /// Adds a session's worker; `wake` is signalled when a frame it waits for is published.
    pub fn subscribe(&self, sid: SessionId, wake: Sender<()>) {
        self.lock().subscribers.insert(
            sid,
            Subscriber {
                wake,
                wants_new: false,
            },
        );
    }

    /// Removes a session's worker. When it was the last one, this wakes the capture thread,
    /// which then drops `latest` if the screen has changed since (screen_changed).
    pub fn unsubscribe(&self, sid: SessionId) {
        let mut inner = self.lock();
        inner.subscribers.remove(&sid);
        if inner.subscribers.is_empty() {
            let _ = self.wake.try_send(());
        }
    }

    /// Sets whether `sid` waits for a newer frame; turning it on wakes the capture thread.
    pub fn set_wants(&self, sid: SessionId, wants: bool) {
        let mut inner = self.lock();
        let Some(sub) = inner.subscribers.get_mut(&sid) else {
            return;
        };
        let was = std::mem::replace(&mut sub.wants_new, wants);
        // Only a rising edge can change what capture decides; workers re-assert this often.
        if wants && !was {
            let _ = self.wake.try_send(());
        }
    }

    /// Wakes the capture thread.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "phase 0 stub: used by event-driven capture (G2 B3)"
        )
    )]
    pub fn wake_capture(&self) {
        let _ = self.wake.try_send(());
    }

    /// True while any session is subscribed.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: used by capture (G2 B4)")
    )]
    pub fn has_subscribers(&self) -> bool {
        !self.lock().subscribers.is_empty()
    }

    /// Drops `latest` if no session is subscribed, so that an idle server holds no frame.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 0 stub: used by capture (G2 B4)")
    )]
    pub fn release_latest(&self) {
        let old = {
            let mut inner = self.lock();
            if !inner.subscribers.is_empty() {
                return;
            }
            inner.stale = false;
            inner.latest.take()
        };
        drop(old);
    }

    /// True when any subscriber waits for a newer frame.
    pub fn demand(&self) -> bool {
        self.lock().subscribers.values().any(|s| s.wants_new)
    }

    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.lock().latest.clone()
    }

    /// True when a frame other than capture `seen` (0: none) is there to encode. Otherwise
    /// marks `sid` as waiting for a newer one, as `set_wants(sid, true)` does. Under one lock
    /// with `publish`, so a frame published after the caller last looked is never missed.
    /// (Addition to the brief's API.)
    pub fn want_newer(&self, sid: SessionId, seen: u64) -> bool {
        let mut inner = self.lock();
        if inner.latest.as_ref().is_some_and(|f| f.seq != seen) {
            return true;
        }
        if let Some(sub) = inner.subscribers.get_mut(&sid) {
            if !std::mem::replace(&mut sub.wants_new, true) {
                let _ = self.wake.try_send(());
            }
        }
        false
    }

    /// Whether the screen has changed since `latest` was grabbed, with nobody asking for a
    /// newer frame since. (Addition to the brief's API.)
    pub fn latest_is_stale(&self) -> bool {
        self.lock().stale
    }

    /// Replaces `latest` and wakes every waiting subscriber, clearing its flag. Returns the
    /// published frame.
    pub fn publish(&self, frame: Frame) -> Arc<Frame> {
        let frame = Arc::new(frame);
        let old = {
            let mut inner = self.lock();
            for sub in inner.subscribers.values_mut().filter(|s| s.wants_new) {
                sub.wants_new = false;
                let _ = sub.wake.try_send(());
            }
            inner.stale = false;
            inner.latest.replace(Arc::clone(&frame))
        };
        // The old frame's buffer goes back to the pool outside the lock.
        drop(old);
        frame
    }

    /// Drops `latest` after a resize: workers then wait for a grab at the new size instead of
    /// encoding the old one first. (Addition to the brief's API.)
    pub fn invalidate(&self) {
        let old = {
            let mut inner = self.lock();
            inner.stale = false;
            inner.latest.take()
        };
        drop(old);
    }

    /// Capture calls this while damage is pending that nobody waits for. With no session
    /// subscribed it drops `latest`, so the next viewer starts from a fresh grab (brief 5.2).
    /// Subscribed sessions keep it but it is marked stale: their demand lapses between frames
    /// (the frame rate cap), and then the newest frame is what they should encode, but also
    /// while they are out of credit or paused, and a worker coming back from that, or a new
    /// session, asks for a fresh grab first (see `latest_is_stale`). (Addition to the brief's
    /// API.)
    pub fn screen_changed(&self) {
        let old = {
            let mut inner = self.lock();
            if !inner.subscribers.is_empty() {
                inner.stale = inner.latest.is_some();
                return;
            }
            inner.stale = false;
            inner.latest.take()
        };
        drop(old);
    }

    /// The full root size (not even-cropped), as last seen by capture.
    pub fn screen_size(&self) -> (u32, u32) {
        self.lock().screen
    }

    pub fn set_screen_size(&self, width: u32, height: u32) {
        self.lock().screen = (width, height);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::convert::FramePool;

    #[test]
    fn demand_follows_wants_and_wakes_capture_on_the_rising_edge() {
        let (hub, wake) = FrameHub::new();
        let (tx, _rx) = crossbeam_channel::bounded(1);
        hub.subscribe(1, tx.clone());
        hub.subscribe(2, tx);
        assert!(!hub.demand() && wake.try_recv().is_err());

        hub.set_wants(1, true);
        assert!(hub.demand());
        assert!(wake.try_recv().is_ok());
        hub.set_wants(1, true);
        hub.set_wants(2, true);
        assert!(
            wake.try_recv().is_ok(),
            "the second subscriber's rising edge wakes too"
        );
        hub.set_wants(1, true);
        assert!(wake.try_recv().is_err(), "no wake without a rising edge");

        hub.set_wants(1, false);
        assert!(hub.demand());
        hub.unsubscribe(2);
        assert!(!hub.demand());
        assert!(wake.try_recv().is_err(), "a subscriber is left");
        hub.unsubscribe(1);
        assert!(
            wake.try_recv().is_ok(),
            "the last one leaving wakes capture, to drop a stale `latest`"
        );
        // Unknown sessions are ignored.
        hub.set_wants(9, true);
        assert!(!hub.demand() && wake.try_recv().is_err());
    }

    fn frame(pool: &FramePool, seq: u64) -> Frame {
        Frame {
            seq,
            capture_us: seq * 10,
            image: pool.get(64, 32),
        }
    }

    #[test]
    fn publish_notifies_only_waiting_subscribers_once() {
        let (hub, _wake) = FrameHub::new();
        let (tx1, rx1) = crossbeam_channel::bounded(1);
        let (tx2, rx2) = crossbeam_channel::bounded(1);
        hub.subscribe(1, tx1);
        hub.subscribe(2, tx2);
        hub.set_wants(1, true);
        let pool = FramePool::new();
        let frame = |seq| frame(&pool, seq);

        hub.publish(frame(1));
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
        assert!(!hub.demand(), "publishing clears the flag");
        hub.publish(frame(2));
        assert!(rx1.try_recv().is_err(), "one NewFrame per wish");
        let latest = hub.latest().unwrap();
        assert_eq!(
            (latest.seq, latest.image.width, latest.image.height),
            (2, 64, 32)
        );

        // Subscribed sessions keep `latest` through unwatched damage; without any it goes.
        hub.screen_changed();
        assert_eq!(hub.latest().map(|f| f.seq), Some(2));
        hub.unsubscribe(1);
        hub.unsubscribe(2);
        hub.screen_changed();
        assert!(hub.latest().is_none() && !hub.latest_is_stale());
        hub.publish(frame(3));
        hub.invalidate();
        assert!(hub.latest().is_none());
        // Workers still holding the frame keep it alive.
        assert_eq!(latest.capture_us, 20);

        hub.set_screen_size(1365, 767);
        assert_eq!(hub.screen_size(), (1365, 767));
    }

    #[test]
    fn a_frame_published_between_looking_and_asking_is_not_missed() {
        // Pipeline P6: worker A encoded seq 1 and B waits; capture publishes 2 for B after A
        // looked at `latest` but before A asks for a newer frame.
        let (hub, wake) = FrameHub::new();
        let (tx_a, rx_a) = crossbeam_channel::bounded(1);
        let (tx_b, rx_b) = crossbeam_channel::bounded(1);
        hub.subscribe(1, tx_a);
        hub.subscribe(2, tx_b);
        let pool = FramePool::new();
        hub.set_wants(1, true);
        hub.publish(frame(&pool, 1));
        assert!(rx_a.try_recv().is_ok());
        hub.set_wants(2, true);
        let seen = hub.latest().unwrap().seq;
        hub.publish(frame(&pool, 2));
        assert!(rx_b.try_recv().is_ok());
        let _ = wake.try_recv();
        assert!(hub.want_newer(1, seen), "seq 2 is there for A");
        assert!(!hub.demand() && wake.try_recv().is_err());
        // With nothing newer, A is marked as waiting and capture is woken.
        assert!(!hub.want_newer(1, 2));
        assert!(hub.demand() && wake.try_recv().is_ok());
        hub.publish(frame(&pool, 3));
        assert!(rx_a.try_recv().is_ok());
        // Before any frame, anything published counts as newer.
        let (hub, _wake) = FrameHub::new();
        hub.subscribe(1, crossbeam_channel::bounded(1).0);
        assert!(!hub.want_newer(1, 0) && hub.demand());
        hub.publish(frame(&pool, 1));
        assert!(hub.want_newer(1, 0));
    }

    #[test]
    fn latest_is_released_only_without_subscribers() {
        let (hub, wake) = FrameHub::new();
        let pool = FramePool::new();
        assert!(!hub.has_subscribers());
        hub.wake_capture();
        assert!(wake.try_recv().is_ok());
        let published = hub.publish(frame(&pool, 1));
        assert!(Arc::ptr_eq(&published, &hub.latest().unwrap()));
        hub.subscribe(1, crossbeam_channel::bounded(1).0);
        assert!(hub.has_subscribers());
        hub.release_latest();
        assert_eq!(
            hub.latest().map(|f| f.seq),
            Some(1),
            "a subscriber keeps it"
        );
        hub.unsubscribe(1);
        hub.release_latest();
        assert!(hub.latest().is_none() && !hub.has_subscribers());
    }

    #[test]
    fn unwatched_damage_marks_latest_stale_until_the_next_grab() {
        // Pipeline P5: a paused or blocked session keeps `latest`, but it no longer shows the
        // screen.
        let (hub, _wake) = FrameHub::new();
        hub.subscribe(1, crossbeam_channel::bounded(1).0);
        let pool = FramePool::new();
        hub.screen_changed();
        assert!(!hub.latest_is_stale(), "nothing to be stale");
        hub.set_wants(1, true);
        hub.publish(frame(&pool, 1));
        hub.set_wants(1, false);
        assert!(!hub.latest_is_stale());
        hub.screen_changed();
        assert!(hub.latest_is_stale());
        assert_eq!(hub.latest().map(|f| f.seq), Some(1));
        hub.publish(frame(&pool, 2));
        assert!(!hub.latest_is_stale());
        hub.screen_changed();
        hub.invalidate();
        assert!(!hub.latest_is_stale() && hub.latest().is_none());
    }
}
