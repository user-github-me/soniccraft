//! `notifications/progress` for long renders.
//!
//! The engine reports [`ProgressEvent`]s through a synchronous callback that must never block. The
//! callback maps each event to a percentage (strictly increasing, as MCP requires) and queues it; a
//! separate task sends the queued notifications, coalescing bursts. The render therefore never waits on
//! the client, and in particular never blocks in rmcp's `notify_progress`, which stalls once the client
//! has closed its input.
//!
//! [`ProgressReporter::finish`] stops the callback and flushes what is queued before the tool returns,
//! so every notification precedes the response; [`ProgressCloser::close`] (on cancellation) stops it
//! without flushing, so nothing more is sent for a cancelled request.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rmcp::Peer;
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::service::RoleServer;
use soniccraft_engine::{ProgressCallback, ProgressEvent};
use tokio::sync::mpsc;

/// The scale `progress` is reported on (`total` of every notification).
pub(crate) const PROGRESS_TOTAL: f64 = 100.0;

/// How long [`ProgressReporter::finish`] waits for queued notifications to be written.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Highest value sent; leaves room above the last stage so every later event can still increase.
const PROGRESS_CEILING: f64 = 99.9;

/// Where the synthesis stage starts and how much of the scale it spans (synthesis dominates real renders).
const SYNTH_START: f64 = 5.0;
const SYNTH_SPAN: f64 = 55.0;

/// "Waiting for a render slot" notifications climb from here in steps of 0.01, staying below the first
/// stage (`parse`, 1.0).
const WAIT_START: f64 = 0.1;
const WAIT_CEILING: f64 = 0.99;

/// The point on the 0–100 scale at which an engine pipeline stage starts, in the engine's order: parse,
/// decode (clip sources), synthesize, process (cue DSP), layout, mix, master, encode, export.
fn stage_floor(stage: &str) -> Option<f64> {
    Some(match stage {
        "parse" | "validate" => 1.0,
        "decode" => 2.0,
        "synthesize" => SYNTH_START,
        "process" => 60.0,
        "layout" => 68.0,
        "mix" => 72.0,
        "master" => 80.0,
        "encode" => 90.0,
        "export" => 96.0,
        _ => return None,
    })
}

/// Maps engine events onto a strictly increasing percentage.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    last: Option<f64>,
}

impl Tracker {
    /// The progress value and message for `event`, or `None` when the scale is exhausted.
    pub(crate) fn update(&mut self, event: &ProgressEvent) -> Option<(f64, String)> {
        // `if let` rather than `match`: the engine may add event kinds, which then count as unknown stages.
        let (target, message) = if let ProgressEvent::Stage { stage, detail } = event {
            let message = if detail.trim().is_empty() {
                capitalize(stage)
            } else {
                format!("{}: {}", capitalize(stage), detail.trim())
            };
            (stage_floor(stage), message)
        } else if let ProgressEvent::Synthesized {
            cue_id,
            cached,
            done,
            total,
        } = event
        {
            let fraction = if *total == 0 {
                1.0
            } else {
                (*done).min(*total) as f64 / *total as f64
            };
            let verb = if *cached {
                "Loaded from cache"
            } else {
                "Synthesized"
            };
            (
                Some(SYNTH_START + SYNTH_SPAN * fraction),
                format!("{verb} {cue_id} ({done}/{total})"),
            )
        } else {
            (None, "Working".to_string())
        };
        let floor = self.last.map_or(0.0, |last| round2(last + 0.01));
        let value = match target {
            Some(target) if round2(target) >= floor => round2(target),
            // Unknown stages and out-of-order events still move forward a little.
            Some(_) => floor,
            None => round2(self.last.map_or(0.5, |last| last + 0.5)),
        };
        if value > PROGRESS_CEILING || self.last.is_some_and(|last| value <= last) {
            return None;
        }
        self.last = Some(value);
        Some((value, message))
    }
}

impl Tracker {
    /// The progress value for a "still waiting for a render slot" message, or `None` once the waiting
    /// range is used up (or the render has started).
    pub(crate) fn waiting(&mut self, message: &str) -> Option<(f64, String)> {
        let value = round2(self.last.map_or(WAIT_START, |last| last + 0.01));
        if value > WAIT_CEILING || self.last.is_some_and(|last| value <= last) {
            return None;
        }
        self.last = Some(value);
        Some((value, message.to_string()))
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn capitalize(text: &str) -> String {
    let mut chars = text.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

enum Message {
    Update(f64, String),
    Done,
}

struct State {
    tracker: Tracker,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    tx: mpsc::UnboundedSender<Message>,
}

impl Shared {
    fn on_event(&self, event: &ProgressEvent) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return;
        }
        if let Some((value, message)) = state.tracker.update(event) {
            // Sending under the lock keeps queue order equal to value order.
            let _ = self.tx.send(Message::Update(value, message));
        }
    }

    fn on_waiting(&self, message: &str) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return;
        }
        if let Some((value, message)) = state.tracker.waiting(message) {
            let _ = self.tx.send(Message::Update(value, message));
        }
    }

    /// Stop accepting events; with `flush`, queue the end marker so the forwarder drains and exits.
    fn close(&self, flush: bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.closed {
            state.closed = true;
            if flush {
                let _ = self.tx.send(Message::Done);
            }
        }
    }
}

/// Progress reporting for one tool call.
pub(crate) struct ProgressReporter {
    shared: Arc<Shared>,
    forwarder: Option<tokio::task::JoinHandle<()>>,
}

impl ProgressReporter {
    /// Start forwarding progress for the request that carried `token`.
    pub(crate) fn start(peer: Peer<RoleServer>, token: ProgressToken) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                tracker: Tracker::default(),
                closed: false,
            }),
            tx,
        });
        let forwarder = tokio::spawn(forward(peer, token, rx));
        ProgressReporter {
            shared,
            forwarder: Some(forwarder),
        }
    }

    /// The engine callback.
    pub(crate) fn callback(&self) -> ProgressCallback {
        let shared = self.shared.clone();
        Arc::new(move |event: &ProgressEvent| shared.on_event(event))
    }

    /// Report that the render is waiting for a slot (before the engine starts).
    pub(crate) fn waiting(&self, message: &str) {
        self.shared.on_waiting(message);
    }

    /// A handle that can stop the reporter from another task (the cancellation watcher).
    pub(crate) fn closer(&self) -> ProgressCloser {
        ProgressCloser(self.shared.clone())
    }

    /// Stop the callback and wait (briefly) until every queued notification has been written.
    pub(crate) async fn finish(mut self) {
        self.shared.close(true);
        if let Some(mut forwarder) = self.forwarder.take()
            && tokio::time::timeout(FLUSH_TIMEOUT, &mut forwarder)
                .await
                .is_err()
        {
            tracing::debug!("progress notifications did not flush in time; dropping the rest");
            forwarder.abort();
        }
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.shared.close(false);
        if let Some(forwarder) = self.forwarder.take() {
            forwarder.abort();
        }
    }
}

/// Stops a [`ProgressReporter`] without flushing (used when the request is cancelled).
#[derive(Clone)]
pub(crate) struct ProgressCloser(Arc<Shared>);

impl ProgressCloser {
    pub(crate) fn close(&self) {
        self.0.close(false);
    }
}

async fn forward(
    peer: Peer<RoleServer>,
    token: ProgressToken,
    mut rx: mpsc::UnboundedReceiver<Message>,
) {
    while let Some(message) = rx.recv().await {
        let Message::Update(mut value, mut text) = message else {
            return;
        };
        // Coalesce a burst (e.g. hundreds of cached cues) into its latest state.
        let mut done = false;
        loop {
            match rx.try_recv() {
                Ok(Message::Update(v, t)) => {
                    value = v;
                    text = t;
                }
                Ok(Message::Done) => {
                    done = true;
                    break;
                }
                Err(_) => break,
            }
        }
        let param = ProgressNotificationParam::new(token.clone(), value)
            .with_total(PROGRESS_TOTAL)
            .with_message(text);
        if let Err(error) = peer.notify_progress(param).await {
            tracing::debug!(%error, "stopping progress notifications");
            return;
        }
        if done {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(name: &str) -> ProgressEvent {
        ProgressEvent::Stage {
            stage: name.into(),
            detail: String::new(),
        }
    }

    fn synth(done: usize, total: usize) -> ProgressEvent {
        ProgressEvent::Synthesized {
            cue_id: format!("cue_{done}"),
            cached: done.is_multiple_of(2),
            done,
            total,
        }
    }

    #[test]
    fn values_strictly_increase() {
        let mut tracker = Tracker::default();
        let events = [
            stage("parse"),
            stage("decode"),
            stage("synthesize"),
            synth(1, 3),
            synth(2, 3),
            synth(2, 3), // duplicate
            synth(3, 3),
            stage("process"),
            stage("synthesize"), // out of order
            stage("unknown-stage"),
            stage("layout"),
            stage("mix"),
            stage("master"),
            stage("encode"),
            stage("export"),
        ];
        let mut last = f64::MIN;
        for event in &events {
            let (value, message) = tracker.update(event).expect("progress left");
            assert!(value > last, "{value} after {last} for {event:?}");
            assert!(value <= PROGRESS_CEILING);
            assert!(!message.is_empty());
            last = value;
        }
    }

    #[test]
    fn messages_are_readable() {
        let mut tracker = Tracker::default();
        let (_, message) = tracker
            .update(&ProgressEvent::Stage {
                stage: "master".into(),
                detail: "EBU R128 pass 2".into(),
            })
            .unwrap();
        assert_eq!(message, "Master: EBU R128 pass 2");
        let (value, message) = tracker
            .update(&ProgressEvent::Synthesized {
                cue_id: "cue_01".into(),
                cached: false,
                done: 0,
                total: 0,
            })
            .unwrap();
        assert!(
            value > 80.0,
            "out-of-order synthesis still increases: {value}"
        );
        assert_eq!(message, "Synthesized cue_01 (0/0)");
    }

    #[test]
    fn waiting_stays_below_the_first_stage() {
        let mut tracker = Tracker::default();
        let mut values = Vec::new();
        while let Some((value, message)) = tracker.waiting("Waiting for another render to finish") {
            assert_eq!(message, "Waiting for another render to finish");
            values.push(value);
        }
        assert_eq!(values.first(), Some(&WAIT_START));
        assert_eq!(values.last(), Some(&WAIT_CEILING));
        assert!(values.windows(2).all(|w| w[1] > w[0]), "{values:?}");
        // The render then starts and its stages still increase.
        let (value, _) = tracker.update(&stage("parse")).unwrap();
        assert!(value > WAIT_CEILING);
        assert!(tracker.waiting("late").is_none());
    }

    #[test]
    fn exhausted_scale_stops_sending() {
        let mut tracker = Tracker {
            last: Some(PROGRESS_CEILING),
        };
        assert!(tracker.update(&stage("export")).is_none());
    }
}
