//! Cooperative cancellation and the blocking-pool helpers.
//!
//! A [`Cancel`] wraps the optional [`CancelFlag`] of a render. Synchronous DSP loops poll
//! [`Cancel::check`] at their checkpoints (per cue, per source, per track); async stages race their future
//! against [`Cancel::cancelled`], which polls the flag every [`POLL_INTERVAL`]. CPU-heavy work runs through
//! [`blocking`] on Tokio's blocking pool so the async runtime (e.g. the MCP server) stays responsive.

use std::future::Future;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::CancelFlag;

/// How often an awaiting render looks at the cancellation flag.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(15);

/// Why a render stage stopped early.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stop {
    /// The cancellation flag was set.
    Cancelled,
    /// The stage failed; the message summarizes it (details are in the job's diagnostics).
    Failed(String),
}

impl Stop {
    /// A failure with `message`.
    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Stop::Failed(message.into())
    }
}

/// The cancellation state of one render (cheap to clone; `None` never cancels).
#[derive(Debug, Clone, Default)]
pub(crate) struct Cancel(Option<CancelFlag>);

impl Cancel {
    /// Wrap an optional flag.
    pub(crate) fn new(flag: Option<CancelFlag>) -> Self {
        Cancel(flag)
    }

    /// True once the flag has been set.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.as_ref().is_some_and(|f| f.load(Ordering::Relaxed))
    }

    /// `Err(Stop::Cancelled)` once the flag has been set.
    pub(crate) fn check(&self) -> Result<(), Stop> {
        if self.is_cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Resolves when the flag is set; never resolves without a flag.
    pub(crate) async fn cancelled(&self) {
        let Some(flag) = &self.0 else {
            return std::future::pending().await;
        };
        while !flag.load(Ordering::Relaxed) {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Run `future` unless the flag is set first. A cancelled future is dropped, which aborts in-flight
    /// HTTP requests and kills child processes; work already handed to the blocking pool finishes in the
    /// background and its result is discarded.
    pub(crate) async fn race<F: Future>(&self, future: F) -> Result<F::Output, Stop> {
        self.check()?;
        if self.0.is_none() {
            return Ok(future.await);
        }
        tokio::select! {
            output = future => Ok(output),
            () = self.cancelled() => Err(Stop::Cancelled),
        }
    }
}

/// Run `work` on Tokio's blocking pool. A panic inside it becomes a [`Stop::Failed`] instead of
/// propagating.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T, Stop>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Stop> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(join) => Err(Stop::failed(format!(
            "internal error: a processing task failed ({join})"
        ))),
    }
}

/// Map `items` with `f` on up to `workers` threads (scoped, inside the calling blocking task). The output
/// order equals the input order, so results never depend on scheduling.
pub(crate) fn par_map<T, R, F>(items: Vec<T>, workers: usize, f: F) -> Vec<Option<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    let n = items.len();
    let workers = workers.clamp(1, n.max(1));
    if workers == 1 {
        return items.into_iter().map(|t| Some(f(t))).collect();
    }
    let inputs: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    let outputs: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let item = inputs[i]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if let Some(item) = item {
                        let result = f(item);
                        *outputs[i]
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                    }
                }
            });
        }
    });
    outputs
        .into_iter()
        .map(|m| {
            m.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .collect()
}

/// Worker threads for [`par_map`]: the available parallelism, at most 8.
pub(crate) fn workers() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .clamp(1, 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn par_map_keeps_order() {
        let out = par_map((0..100).collect(), 4, |x: i32| x * 2);
        let out: Vec<i32> = out.into_iter().map(|x| x.unwrap()).collect();
        assert_eq!(out, (0..100).map(|x| x * 2).collect::<Vec<_>>());
        assert!(par_map(Vec::<i32>::new(), 4, |x| x).is_empty());
        assert_eq!(par_map(vec![7], 4, |x: i32| x + 1), vec![Some(8)]);
    }

    #[test]
    fn cancel_check() {
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = Cancel::new(Some(flag.clone()));
        assert!(cancel.check().is_ok());
        flag.store(true, Ordering::Relaxed);
        assert_eq!(cancel.check(), Err(Stop::Cancelled));
        assert!(Cancel::default().check().is_ok());
    }

    #[tokio::test]
    async fn race_stops_a_pending_future() {
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = Cancel::new(Some(flag.clone()));
        let setter = {
            let flag = flag.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                flag.store(true, Ordering::Relaxed);
            })
        };
        let result = cancel.race(std::future::pending::<()>()).await;
        assert_eq!(result, Err(Stop::Cancelled));
        setter.await.unwrap();
        assert_eq!(Cancel::default().race(async { 5 }).await, Ok(5));
    }

    #[tokio::test]
    async fn blocking_turns_panics_into_failures() {
        let r: Result<(), Stop> = blocking(|| panic!("boom")).await;
        assert!(matches!(r, Err(Stop::Failed(m)) if m.contains("internal error")));
        assert_eq!(blocking(|| Ok(3)).await, Ok(3));
    }
}
