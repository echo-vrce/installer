// SPDX-License-Identifier: GPL-3.0-or-later
//! Machine-readable progress, for when the reader is another copy of this program.
//!
//! The elevated run reports back through a log file, and prose is all a log file usually
//! needs. But the window on the other end wants to draw the same checklist and the same
//! progress bar it draws for an ordinary run, and it cannot get those out of sentences
//! without parsing its own output - which is the kind of thing that works until someone
//! rewords a message.
//!
//! So when `--events` is on, one JSON object per line goes into the log beside the prose.
//! The prose stays because a person still reads that file afterwards; the objects are
//! ignored by anyone who is not looking for them, because they are on their own lines and
//! start with a brace.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;

/// What one end tells the other. Deliberately few: every one of these maps onto something
/// the window already knows how to draw.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A named phase began.
    Stage(String),
    /// Bytes moved for one named thing. `total` is absent when the server did not say.
    Progress { what: String, done: u64, total: Option<u64> },
    /// Item `index` of `of` started.
    Item { name: String, index: usize, of: usize },
    /// The operation ended.
    Done { ok: bool, summary: String },
}

impl Event {
    fn to_line(&self) -> String {
        let v = match self {
            Event::Stage(text) => json!({"e": "stage", "text": text}),
            Event::Progress { what, done, total } => {
                json!({"e": "progress", "what": what, "done": done, "total": total})
            }
            Event::Item { name, index, of } => {
                json!({"e": "item", "name": name, "index": index, "of": of})
            }
            Event::Done { ok, summary } => json!({"e": "done", "ok": ok, "summary": summary}),
        };
        v.to_string()
    }

    /// The other direction. Anything unrecognised is `None` so the reader can treat the
    /// line as ordinary text, which is what a prose line is.
    pub fn parse(line: &str) -> Option<Event> {
        let line = line.trim();
        if !line.starts_with('{') {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        match v.get("e")?.as_str()? {
            "stage" => Some(Event::Stage(v.get("text")?.as_str()?.to_string())),
            "progress" => Some(Event::Progress {
                what: v.get("what").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                done: v.get("done")?.as_u64()?,
                total: v.get("total").and_then(|x| x.as_u64()),
            }),
            "item" => Some(Event::Item {
                name: v.get("name")?.as_str()?.to_string(),
                index: v.get("index")?.as_u64()? as usize,
                of: v.get("of")?.as_u64()? as usize,
            }),
            "done" => Some(Event::Done {
                ok: v.get("ok")?.as_bool()?,
                summary: v.get("summary").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            }),
            _ => None,
        }
    }
}

static SINK: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Starts writing events to `path`, which is the same file the prose goes to.
pub fn to(path: PathBuf) {
    if let Ok(mut guard) = SINK.lock() {
        *guard = Some(path);
    }
}

/// Sends one event, if anyone asked for them. A no-op otherwise, which is the ordinary case.
///
/// Opened and closed per line rather than held: the reader is another process tailing the
/// same file, and a buffered writer would mean progress arriving in bursts long after it
/// happened.
pub fn emit(event: &Event) {
    let Ok(guard) = SINK.lock() else { return };
    let Some(path) = guard.as_ref() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", event.to_line());
        let _ = f.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_survives_the_round_trip() {
        for e in [
            Event::Stage("Extracting".into()),
            Event::Progress { what: "a.zip".into(), done: 12, total: Some(345) },
            Event::Progress { what: String::new(), done: 1, total: None },
            Event::Item { name: "b.dll".into(), index: 3, of: 19 },
            Event::Done { ok: true, summary: "19 fetched".into() },
        ] {
            let line = e.to_line();
            assert_eq!(Event::parse(&line), Some(e.clone()), "line was {line}");
        }
    }

    #[test]
    fn prose_is_not_mistaken_for_an_event() {
        // The two share a file, so telling them apart has to be reliable in the direction
        // that matters: a sentence must never decode as progress.
        for line in [
            "07:11:02  == UPDATE",
            "07:11:02  ok: 19 fetched, 0 removed",
            "",
            "   ",
            "{not json at all",
            r#"{"e":"unknown"}"#,
            r#"{"no":"e field"}"#,
        ] {
            assert_eq!(Event::parse(line), None, "line was {line:?}");
        }
    }

    #[test]
    fn emitting_without_a_sink_does_nothing() {
        emit(&Event::Stage("nobody is listening".into()));
    }
}
