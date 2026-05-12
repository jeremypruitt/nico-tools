//! PRD-007 Slice 2 — correlate runner adapter (streaming + cancellation).
//!
//! The popup is built on top of an asynchronous stream of
//! [`CorrelateUpdate`]s instead of one bulk `Action::CorrelateResults`:
//! Diagnosis renders the moment fast Sources resolve, per-source events
//! stream into the timeline as they land, and the source-availability
//! dots transition `⟳ → ● / ✗` as Sources report. The popup re-renders on
//! every update.
//!
//! Lifecycle is `Drop`-safe. The per-Source futures live inside one
//! driver task; dropping the [`CorrelateStream`] aborts that task, which
//! cancels every still-running `Source::collect` future in lockstep. No
//! errors are logged on cancel — cancellation is the happy path the
//! popup-dismiss flow walks every time.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::{FuturesUnordered, StreamExt};
use nico_correlate::diagnosis::DiagnosisConfig;
use nico_correlate::source::{Source, SourceResult, StateEntry};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::model::{EntityRef, PopoverDiagnosis, PopoverEvent, PopoverSeverity};

/// Friendly alias for the (`SourceKind::name()`) static strings the
/// stream emits. Kept as `&'static str` so the action-channel payload
/// stays cheap and `Copy`-ish.
pub type SourceName = &'static str;

/// One increment of an in-flight correlate run. The popup re-renders on
/// every update; ordering is:
///
/// 1. Exactly one [`Loading`](CorrelateUpdate::Loading) at the start,
///    listing the Sources that will be queried.
/// 2. One [`SourceLanded`](CorrelateUpdate::SourceLanded) or
///    [`SourceFailed`](CorrelateUpdate::SourceFailed) per Source, in
///    completion order (fast Sources first; Loki typically last).
/// 3. Exactly one [`Diagnosis`](CorrelateUpdate::Diagnosis) after the
///    last Source lands. `None` when the pattern matcher had nothing
///    to say.
/// 4. Exactly one [`Done`](CorrelateUpdate::Done) terminator.
#[derive(Debug, Clone, PartialEq)]
pub enum CorrelateUpdate {
    Loading {
        sources: Vec<SourceName>,
    },
    SourceLanded {
        source: SourceName,
        events: Vec<PopoverEvent>,
    },
    SourceFailed {
        source: SourceName,
        reason: String,
    },
    Diagnosis {
        diagnosis: Option<PopoverDiagnosis>,
    },
    Done,
}

/// A `Drop`-safe stream of [`CorrelateUpdate`]s tied to a single
/// correlate run. The internal driver task is aborted on drop, which
/// cancels every still-running `Source::collect` future without panics
/// or task leaks.
pub struct CorrelateStream {
    rx: mpsc::Receiver<CorrelateUpdate>,
    driver: Option<JoinHandle<()>>,
}

impl Drop for CorrelateStream {
    fn drop(&mut self) {
        if let Some(h) = self.driver.take() {
            h.abort();
        }
    }
}

impl futures::Stream for CorrelateStream {
    type Item = CorrelateUpdate;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// PRD-007 Slice 2: stream a correlate run for `entity` over the given
/// prepared `sources`. Each Source's `collect` call runs concurrently
/// inside a [`FuturesUnordered`]; completion order drives `SourceLanded`
/// / `SourceFailed` emission. After the last Source lands, Diagnosis
/// runs over the accumulated events + state and is yielded, followed by
/// the terminal `Done`.
///
/// The returned stream is `Drop`-safe — see [`CorrelateStream`].
pub fn run_correlate(
    entity: EntityRef,
    sources: Vec<(SourceName, Box<dyn Source>)>,
    diagnosis_config: DiagnosisConfig,
) -> CorrelateStream {
    let (tx, rx) = mpsc::channel(32);
    let source_names: Vec<SourceName> = sources.iter().map(|(n, _)| *n).collect();

    let driver = tokio::spawn(async move {
        if tx
            .send(CorrelateUpdate::Loading {
                sources: source_names,
            })
            .await
            .is_err()
        {
            return;
        }

        let mut in_flight = FuturesUnordered::new();
        for (name, source) in sources {
            let id = entity.id.clone();
            let id_type = entity.id_type.clone();
            in_flight.push(async move {
                let result = source.collect(&id, &id_type).await;
                (name, result)
            });
        }

        let mut events_acc: Vec<nico_correlate::Event> = Vec::new();
        let mut state_acc: Vec<StateEntry> = Vec::new();

        while let Some((name, result)) = in_flight.next().await {
            match result {
                SourceResult::Output(o) => {
                    let popover_events: Vec<PopoverEvent> = o
                        .events
                        .iter()
                        .map(|e| PopoverEvent {
                            ts: e.ts,
                            source: e.source.clone(),
                            kind: e.kind.clone(),
                            message: e.message.clone(),
                            severity: severity_to_popover(&e.severity),
                        })
                        .collect();
                    events_acc.extend(o.events);
                    state_acc.extend(o.state);
                    if tx
                        .send(CorrelateUpdate::SourceLanded {
                            source: name,
                            events: popover_events,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                SourceResult::Unavailable(u) => {
                    if tx
                        .send(CorrelateUpdate::SourceFailed {
                            source: name,
                            reason: u.reason,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }

        let diagnosis =
            nico_correlate::diagnosis::diagnose(&events_acc, &state_acc, &diagnosis_config).map(
                |d| PopoverDiagnosis {
                    pattern: d.pattern,
                    error_signature: d.error_signature,
                    next_commands: d.next_commands,
                },
            );
        if tx
            .send(CorrelateUpdate::Diagnosis { diagnosis })
            .await
            .is_err()
        {
            return;
        }
        let _ = tx.send(CorrelateUpdate::Done).await;
    });

    CorrelateStream {
        rx,
        driver: Some(driver),
    }
}

fn severity_to_popover(s: &nico_correlate::event::Severity) -> PopoverSeverity {
    match s {
        nico_correlate::event::Severity::Info => PopoverSeverity::Info,
        nico_correlate::event::Severity::Warning => PopoverSeverity::Warning,
        nico_correlate::event::Severity::Error => PopoverSeverity::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use nico_correlate::event::{Event, Severity};
    use nico_correlate::id::IdType;
    use nico_correlate::source::{SourceOutput, SourceUnavailable};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::time::sleep;

    fn entity_for_test() -> EntityRef {
        EntityRef {
            id: "dpu-r12u5".into(),
            id_type: IdType::Dpu,
        }
    }

    /// Mock Source that emits `events` after waiting `delay`. The
    /// returned event is tagged with the source name so tests can pin
    /// which Source contributed which event.
    struct DelayedSource {
        name: SourceName,
        delay: Duration,
        events: Vec<Event>,
        state: Vec<StateEntry>,
        /// Flips to `true` when `collect` is dropped before completing.
        /// Tracks cancellation observable from inside the Source.
        cancelled: Arc<AtomicBool>,
    }

    impl DelayedSource {
        fn new(name: SourceName, delay: Duration) -> Self {
            Self {
                name,
                delay,
                events: vec![],
                state: vec![],
                cancelled: Arc::new(AtomicBool::new(false)),
            }
        }

        fn with_event(mut self, kind: &str) -> Self {
            self.events.push(Event {
                ts: Utc.with_ymd_and_hms(2026, 5, 12, 10, 0, 0).unwrap(),
                source: self.name.to_string(),
                kind: kind.into(),
                message: format!("{} event", self.name),
                severity: Severity::Info,
                tags: Default::default(),
            });
            self
        }

        fn cancellation_flag(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.cancelled)
        }
    }

    /// `Source::collect` drop-guard: if the future is dropped before
    /// the sleep completes, the flag is set so cancellation tests can
    /// assert that the per-Source future was actually cancelled.
    struct DropGuard {
        flag: Arc<AtomicBool>,
    }
    impl Drop for DropGuard {
        fn drop(&mut self) {
            // Only flips if the guard reaches drop without being
            // explicitly disarmed (see `mem::forget` below).
            self.flag.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Source for DelayedSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn collect(&self, _id: &str, _id_type: &IdType) -> SourceResult {
            let guard = DropGuard {
                flag: Arc::clone(&self.cancelled),
            };
            sleep(self.delay).await;
            // Reached the end without being dropped — disarm the guard
            // so `cancelled` stays `false` on a clean completion.
            std::mem::forget(guard);
            SourceResult::Output(SourceOutput {
                events: self.events.clone(),
                state: self.state.clone(),
            })
        }
    }

    /// Always-unavailable Source: useful for the partial-availability
    /// test path. Carries no delay; reports immediately.
    struct UnavailableMock {
        name: SourceName,
        reason: String,
    }

    #[async_trait]
    impl Source for UnavailableMock {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn collect(&self, _id: &str, _id_type: &IdType) -> SourceResult {
            SourceResult::Unavailable(SourceUnavailable {
                name: self.name,
                reason: self.reason.clone(),
            })
        }
    }

    #[tokio::test]
    async fn stream_yields_loading_first_then_sources_in_completion_order_then_diagnosis_then_done()
    {
        let fast = DelayedSource::new("postgres", Duration::from_millis(10)).with_event("pg_evt");
        let slow = DelayedSource::new("loki", Duration::from_millis(80)).with_event("loki_evt");
        let sources: Vec<(SourceName, Box<dyn Source>)> =
            vec![("postgres", Box::new(fast)), ("loki", Box::new(slow))];

        let mut stream = Box::pin(run_correlate(
            entity_for_test(),
            sources,
            DiagnosisConfig::default(),
        ));

        let first = stream.next().await.expect("Loading");
        assert!(
            matches!(&first, CorrelateUpdate::Loading { sources } if sources == &vec!["postgres", "loki"]),
            "expected Loading, got {first:?}"
        );

        // postgres lands first (10ms), loki second (80ms).
        let second = stream.next().await.expect("first source");
        match second {
            CorrelateUpdate::SourceLanded { source, .. } => assert_eq!(source, "postgres"),
            other => panic!("expected postgres SourceLanded, got {other:?}"),
        }

        let third = stream.next().await.expect("second source");
        match third {
            CorrelateUpdate::SourceLanded { source, .. } => assert_eq!(source, "loki"),
            other => panic!("expected loki SourceLanded, got {other:?}"),
        }

        let fourth = stream.next().await.expect("Diagnosis");
        assert!(
            matches!(fourth, CorrelateUpdate::Diagnosis { .. }),
            "expected Diagnosis"
        );

        let fifth = stream.next().await.expect("Done");
        assert!(matches!(fifth, CorrelateUpdate::Done), "expected Done");

        assert!(
            stream.next().await.is_none(),
            "stream should end after Done"
        );
    }

    #[tokio::test]
    async fn unavailable_source_emits_source_failed_and_does_not_stall_the_run() {
        let fast = DelayedSource::new("postgres", Duration::from_millis(5)).with_event("pg_evt");
        let broken = UnavailableMock {
            name: "loki",
            reason: "LOKI_URL not set".into(),
        };
        let sources: Vec<(SourceName, Box<dyn Source>)> =
            vec![("postgres", Box::new(fast)), ("loki", Box::new(broken))];

        let mut stream = Box::pin(run_correlate(
            entity_for_test(),
            sources,
            DiagnosisConfig::default(),
        ));

        // Loading first
        let _ = stream.next().await.expect("Loading");
        // Two source updates in some order — at least one must be SourceFailed
        // for `loki`.
        let mut saw_failed_loki = false;
        let mut saw_landed_postgres = false;
        for _ in 0..2 {
            match stream.next().await.expect("source update") {
                CorrelateUpdate::SourceFailed { source, reason } => {
                    assert_eq!(source, "loki");
                    assert_eq!(reason, "LOKI_URL not set");
                    saw_failed_loki = true;
                }
                CorrelateUpdate::SourceLanded { source, .. } => {
                    assert_eq!(source, "postgres");
                    saw_landed_postgres = true;
                }
                other => panic!("unexpected update: {other:?}"),
            }
        }
        assert!(saw_failed_loki, "expected SourceFailed for loki");
        assert!(saw_landed_postgres, "expected SourceLanded for postgres");
        // Stream must still emit Diagnosis + Done after the partial run.
        assert!(
            matches!(stream.next().await, Some(CorrelateUpdate::Diagnosis { .. })),
            "expected Diagnosis"
        );
        assert!(matches!(stream.next().await, Some(CorrelateUpdate::Done)));
    }

    #[tokio::test]
    async fn dropping_stream_mid_fetch_cancels_per_source_futures() {
        let slow_a = DelayedSource::new("temporal", Duration::from_secs(60));
        let slow_b = DelayedSource::new("loki", Duration::from_secs(60));
        let flag_a = slow_a.cancellation_flag();
        let flag_b = slow_b.cancellation_flag();

        let sources: Vec<(SourceName, Box<dyn Source>)> =
            vec![("temporal", Box::new(slow_a)), ("loki", Box::new(slow_b))];

        let mut stream = Box::pin(run_correlate(
            entity_for_test(),
            sources,
            DiagnosisConfig::default(),
        ));

        // Wait for Loading to land, then drop mid-fetch.
        let _ = stream.next().await.expect("Loading");
        drop(stream);

        // Give the driver a moment to abort its tasks; both per-Source
        // drop-guards must have observed cancellation.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            flag_a.load(Ordering::SeqCst),
            "temporal Source future should have been dropped on stream cancel"
        );
        assert!(
            flag_b.load(Ordering::SeqCst),
            "loki Source future should have been dropped on stream cancel"
        );
    }

    #[tokio::test]
    async fn empty_sources_still_yields_loading_diagnosis_done() {
        let mut stream = Box::pin(run_correlate(
            entity_for_test(),
            vec![],
            DiagnosisConfig::default(),
        ));
        assert!(matches!(
            stream.next().await,
            Some(CorrelateUpdate::Loading { ref sources }) if sources.is_empty()
        ));
        assert!(matches!(
            stream.next().await,
            Some(CorrelateUpdate::Diagnosis { diagnosis: None })
        ));
        assert!(matches!(stream.next().await, Some(CorrelateUpdate::Done)));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_after_done_does_not_panic() {
        // Sanity: dropping a fully-drained stream is safe (driver
        // already finished; abort is a no-op).
        let mut stream = Box::pin(run_correlate(
            entity_for_test(),
            vec![],
            DiagnosisConfig::default(),
        ));
        while stream.next().await.is_some() {}
        drop(stream);
    }
}
