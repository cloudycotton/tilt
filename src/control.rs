//! Who holds control (brief section 5.6): at most one session, with every change broadcast to
//! all sessions and input released when control moves. The ReleaseAll sent then also cancels
//! the rest of a text the session was typing.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use tokio::sync::watch;

use crate::input::{InputCmd, InputHandle};
use crate::protocol::ServerText;
use crate::session::SessionId;

/// What one session is told: `{"t":"control","you":..,"held":..,"holder":..}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ControlStatus {
    pub you: bool,
    pub held: bool,
    /// Lets a reconnecting page tell its own lost session, which keeps control until the idle
    /// close, from another viewer. (Addition to brief 5.6.)
    pub holder: Option<SessionId>,
}

impl ControlStatus {
    /// The `control` message that tells a session this.
    pub fn message(self) -> ServerText {
        ServerText::Control {
            you: self.you,
            held: self.held,
            holder: self.holder.map(|h| format!("s{h}")),
        }
    }
}

pub struct ControlState {
    input: InputHandle,
    inner: Mutex<Inner>,
}

struct Inner {
    holder: Option<SessionId>,
    members: HashMap<SessionId, watch::Sender<ControlStatus>>,
}

impl ControlState {
    pub fn new(input: InputHandle) -> ControlState {
        ControlState {
            input,
            inner: Mutex::new(Inner {
                holder: None,
                members: HashMap::new(),
            }),
        }
    }

    /// Adds a session. Its writer task forwards every change of the returned receiver.
    pub fn register(&self, sid: SessionId) -> watch::Receiver<ControlStatus> {
        self.lock().register(sid)
    }

    /// Removes a session, releasing control first if it holds it.
    pub fn unregister(&self, sid: SessionId) {
        let mut inner = self.lock();
        let cmds = inner.unregister(sid);
        self.send(cmds);
    }

    /// Makes `sid` the holder; the previous holder's keys and buttons are released, and its
    /// text stops.
    pub fn take(&self, sid: SessionId) {
        let mut inner = self.lock();
        let cmds = inner.take(sid);
        self.send(cmds);
    }

    /// Clears the holder if it is `sid`, releasing what it held and stopping its text.
    pub fn release(&self, sid: SessionId) {
        let mut inner = self.lock();
        let cmds = inner.release(sid);
        self.send(cmds);
    }

    pub fn holder(&self) -> Option<SessionId> {
        self.lock().holder
    }

    /// Queues `cmd` for the input thread if `sid` holds control, and says whether it did.
    /// Checking and queueing under the same lock as `take` orders them: input from a session
    /// that just lost control either precedes the ReleaseAll that `take` sends, or is dropped.
    /// (Replaces the brief's `is_holder`: a check followed by a send can let a press slip in
    /// after that ReleaseAll and stay held.)
    pub fn input_if_holder(&self, sid: SessionId, cmd: InputCmd) -> bool {
        let inner = self.lock();
        let holds = inner.holder == Some(sid);
        if holds {
            self.input.send(cmd);
        }
        holds
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // The state stays consistent across a panic elsewhere: every update is a single step.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sends while the caller still holds the lock, so commands keep the order of the changes.
    fn send(&self, cmds: Vec<InputCmd>) {
        for c in cmds {
            self.input.send(c);
        }
    }
}

impl Inner {
    fn register(&mut self, sid: SessionId) -> watch::Receiver<ControlStatus> {
        let (tx, rx) = watch::channel(self.status_for(sid));
        self.members.insert(sid, tx);
        rx
    }

    fn unregister(&mut self, sid: SessionId) -> Vec<InputCmd> {
        let cmds = self.release(sid);
        self.members.remove(&sid);
        cmds
    }

    fn take(&mut self, sid: SessionId) -> Vec<InputCmd> {
        let prev = self.holder.replace(sid);
        let cmds = match prev {
            Some(p) if p == sid => Vec::new(),
            Some(p) => vec![InputCmd::ReleaseAll { session: p }],
            None => vec![InputCmd::SetControllerPresent(true)],
        };
        self.broadcast();
        cmds
    }

    fn release(&mut self, sid: SessionId) -> Vec<InputCmd> {
        if self.holder != Some(sid) {
            return Vec::new();
        }
        self.holder = None;
        self.broadcast();
        vec![
            InputCmd::ReleaseAll { session: sid },
            InputCmd::SetControllerPresent(false),
        ]
    }

    fn status_for(&self, sid: SessionId) -> ControlStatus {
        ControlStatus {
            you: self.holder == Some(sid),
            held: self.holder.is_some(),
            holder: self.holder,
        }
    }

    /// Updates every member whose view changed; `take` by the holder itself changes nothing.
    fn broadcast(&self) {
        for (&sid, tx) in &self.members {
            let status = self.status_for(sid);
            tx.send_if_modified(|s| std::mem::replace(s, status) != status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner() -> Inner {
        Inner {
            holder: None,
            members: HashMap::new(),
        }
    }

    /// The status of a session that holds control itself (`you`), or sees `holder` hold it.
    fn st(you: bool, holder: Option<SessionId>) -> ControlStatus {
        ControlStatus {
            you,
            held: holder.is_some(),
            holder,
        }
    }

    #[test]
    fn one_holder_at_a_time() {
        let mut c = inner();
        let mut a = c.register(1);
        let mut b = c.register(2);
        let mut v = c.register(3);
        assert_eq!(*a.borrow_and_update(), st(false, None));
        v.borrow_and_update();

        assert_eq!(c.take(1), [InputCmd::SetControllerPresent(true)]);
        assert!(a.has_changed().unwrap() && b.has_changed().unwrap());
        assert_eq!(*a.borrow_and_update(), st(true, Some(1)));
        assert_eq!(*b.borrow_and_update(), st(false, Some(1)));
        assert_eq!(*v.borrow_and_update(), st(false, Some(1)));

        // Taking it again changes nothing and notifies no one.
        assert!(c.take(1).is_empty());
        assert!(!a.has_changed().unwrap() && !b.has_changed().unwrap());

        // Taking it over releases the previous holder's input; presence did not change, but
        // everyone learns who holds it now.
        assert_eq!(c.take(2), [InputCmd::ReleaseAll { session: 1 }]);
        assert_eq!(*a.borrow_and_update(), st(false, Some(2)));
        assert_eq!(*b.borrow_and_update(), st(true, Some(2)));
        assert!(v.has_changed().unwrap());
        assert_eq!(*v.borrow_and_update(), st(false, Some(2)));
        assert_eq!(c.holder, Some(2));

        // Only the holder can release.
        assert!(c.release(1).is_empty());
        assert!(!b.has_changed().unwrap());
        assert_eq!(
            c.release(2),
            [
                InputCmd::ReleaseAll { session: 2 },
                InputCmd::SetControllerPresent(false)
            ]
        );
        assert_eq!(*a.borrow_and_update(), st(false, None));
        assert_eq!(*b.borrow_and_update(), st(false, None));
        assert_eq!(*v.borrow_and_update(), st(false, None));
    }

    #[test]
    fn the_message_names_the_holder_as_its_session_id() {
        assert_eq!(
            st(false, Some(7)).message().to_json(),
            r#"{"t":"control","you":false,"held":true,"holder":"s7"}"#
        );
        assert_eq!(
            st(true, Some(12)).message().to_json(),
            r#"{"t":"control","you":true,"held":true,"holder":"s12"}"#
        );
        assert_eq!(
            st(false, None).message().to_json(),
            r#"{"t":"control","you":false,"held":false}"#
        );
    }

    #[test]
    fn late_joiners_see_the_holder_and_leaving_releases() {
        let mut c = inner();
        let _a = c.register(1);
        c.take(1);
        let mut b = c.register(2);
        assert_eq!(*b.borrow_and_update(), st(false, Some(1)));

        // A viewer leaving changes nothing for the others.
        assert!(c.unregister(2).is_empty());
        assert_eq!(c.holder, Some(1));

        let mut d = c.register(3);
        d.borrow_and_update();
        assert_eq!(
            c.unregister(1),
            [
                InputCmd::ReleaseAll { session: 1 },
                InputCmd::SetControllerPresent(false)
            ]
        );
        assert_eq!(*d.borrow_and_update(), st(false, None));
        assert_eq!(c.holder, None);
        assert!(c.members.len() == 1 && c.members.contains_key(&3));
        // Unknown sessions are harmless.
        assert!(c.unregister(42).is_empty() && c.release(42).is_empty());
    }
}
