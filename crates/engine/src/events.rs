//! Short logs of what happened to a forward or a context, for the TUI's event
//! view and `kprtfwd logs`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const CAPACITY: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at: SystemTime,
    pub message: String,
}

/// The last [`CAPACITY`] events. A message equal to the previous one is not
/// repeated, so a burst of refused connections stays one line.
#[derive(Debug, Clone, Default)]
pub struct EventLog(Arc<Mutex<VecDeque<Event>>>);

impl EventLog {
    pub fn push(&self, message: impl Into<String>) {
        let message = message.into();
        let mut events = self.0.lock().unwrap();
        if events.back().is_some_and(|last| last.message == message) {
            return;
        }
        if events.len() == CAPACITY {
            events.pop_front();
        }
        events.push_back(Event {
            at: SystemTime::now(),
            message,
        });
    }

    pub fn entries(&self) -> Vec<Event> {
        self.0.lock().unwrap().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_latest_and_skips_repeats() {
        let log = EventLog::default();
        log.push("a");
        log.push("a");
        log.push("b");
        log.push("a");
        let messages: Vec<String> = log.entries().into_iter().map(|e| e.message).collect();
        assert_eq!(messages, ["a", "b", "a"]);

        for i in 0..CAPACITY + 5 {
            log.push(i.to_string());
        }
        let entries = log.entries();
        assert_eq!(entries.len(), CAPACITY);
        assert_eq!(entries.last().unwrap().message, (CAPACITY + 4).to_string());
    }
}
