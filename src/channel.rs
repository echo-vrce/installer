// SPDX-License-Identifier: GPL-3.0-or-later
//! Draining a worker's channel from inside a frame.
//!
//! Every screen that owns a background thread does the same thing once per frame: take
//! everything waiting, then act on it. It reads like it should be two lines, and it is -
//! but the obvious spelling does not compile.
//!
//! The messages have to be collected before any of them is handled, because handling one
//! usually means calling `&mut self`, and the receiver being read is a field of that same
//! `self`. Nine copies of that workaround had grown before this existed, and one of them
//! had already been written the wrong way round.

use std::sync::mpsc::{Receiver, TryRecvError};

/// Everything waiting on the channel, plus whether the sender has gone.
///
/// A missing receiver is not disconnected: nothing has been started yet, which is a
/// different thing from a worker that finished or died.
pub fn drain<T>(rx: &Option<Receiver<T>>) -> (Vec<T>, bool) {
    let mut out = Vec::new();
    let Some(rx) = rx else { return (out, false) };
    loop {
        match rx.try_recv() {
            Ok(msg) => out.push(msg),
            Err(TryRecvError::Empty) => return (out, false),
            // Messages already taken are still delivered: a worker that sent its last
            // result and then dropped the sender disconnects in the same breath, and
            // dropping that result would lose the outcome.
            Err(TryRecvError::Disconnected) => return (out, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn nothing_started_is_not_disconnected() {
        let none: Option<Receiver<u8>> = None;
        let (msgs, gone) = drain(&none);
        assert!(msgs.is_empty());
        assert!(!gone, "a channel that was never opened has not been closed");
    }

    #[test]
    fn takes_everything_waiting_without_blocking() {
        let (tx, rx) = mpsc::channel();
        for i in 0..3 {
            tx.send(i).unwrap();
        }
        let (msgs, gone) = drain(&Some(rx));
        assert_eq!(msgs, vec![0, 1, 2]);
        assert!(!gone, "the sender is still alive");
    }

    #[test]
    fn a_final_message_survives_the_sender_being_dropped() {
        // The case that matters: a worker sends its result and returns, so the channel is
        // disconnected by the time anyone looks. Losing that message loses the outcome.
        let (tx, rx) = mpsc::channel();
        tx.send("done").unwrap();
        drop(tx);
        let (msgs, gone) = drain(&Some(rx));
        assert_eq!(msgs, vec!["done"]);
        assert!(gone);
    }

    #[test]
    fn an_empty_closed_channel_reports_only_the_close() {
        let (tx, rx) = mpsc::channel::<u8>();
        drop(tx);
        let (msgs, gone) = drain(&Some(rx));
        assert!(msgs.is_empty());
        assert!(gone);
    }
}
