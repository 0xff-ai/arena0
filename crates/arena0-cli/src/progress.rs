//! Typed coordinated-run progress and its one inline terminal projection.
//!
//! The coordinator records real work here. Indicatif owns the transient stderr
//! line, while Rattles supplies the spinner frames. Full-screen rendering stays
//! in `tui` and machine results stay on stdout.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressFinish, ProgressStyle};
use rattles::{Rattle, presets::braille::Dots};

use crate::ui::Palette;

const ANIMATION_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressMode {
    Interactive,
    Plain,
    Hidden,
}

impl ProgressMode {
    #[must_use]
    pub(crate) const fn for_run(
        human_output: bool,
        use_tui: bool,
        streams_are_terminal: bool,
    ) -> Self {
        if use_tui {
            Self::Hidden
        } else if human_output && streams_are_terminal {
            Self::Interactive
        } else {
            Self::Plain
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStage {
    Connecting,
    Admission,
    Negotiation,
    Activation,
    Execution,
    Verification,
    Replay,
}

impl RunStage {
    const fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting to Hosts",
            Self::Admission => "admitting program",
            Self::Negotiation => "negotiating execution",
            Self::Activation => "activating session",
            Self::Execution => "waiting for execution",
            Self::Verification => "verifying producer receipts",
            Self::Replay => "replaying producer receipts",
        }
    }

    const fn initial(self, total: usize) -> ProgressAmount {
        if matches!(self, Self::Execution) {
            ProgressAmount::Indeterminate
        } else {
            ProgressAmount::Known {
                completed: 0,
                total,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressAmount {
    Known { completed: usize, total: usize },
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunTerminalState {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunProgressState {
    Idle,
    Active {
        stage: RunStage,
        amount: ProgressAmount,
    },
    SuspendedForCallout,
    Terminal(RunTerminalState),
}

#[derive(Debug, Clone)]
pub(crate) struct RunProgress {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    mode: ProgressMode,
    palette: Palette,
    state: RunProgressState,
    animation_ready: bool,
    bar: Option<ProgressBar>,
    #[cfg(test)]
    observations: Vec<RunProgressState>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.clear_bar();
    }
}

impl Inner {
    fn show_bar(&mut self) {
        self.clear_bar();
        let RunProgressState::Active { stage, amount } = self.state else {
            return;
        };
        let message = self.palette.cyan(stage.label());
        let bar = match amount {
            ProgressAmount::Known { completed, total } => {
                let style = ProgressStyle::with_template("{msg} {pos}/{len}")
                    .expect("static progress template is valid");
                let bar =
                    ProgressBar::with_draw_target(Some(total as u64), ProgressDrawTarget::stderr())
                        .with_finish(ProgressFinish::AndClear)
                        .with_style(style)
                        .with_message(message);
                bar.set_position(completed as u64);
                bar
            }
            ProgressAmount::Indeterminate => {
                let style = spinner_style(self.palette);
                let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
                    .with_finish(ProgressFinish::AndClear)
                    .with_style(style)
                    .with_message(message);
                bar.enable_steady_tick(Dots::INTERVAL);
                bar
            }
        };
        self.bar = Some(bar);
    }

    fn clear_bar(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.disable_steady_tick();
            bar.finish_and_clear();
        }
    }

    fn record_observation(&mut self) {
        #[cfg(test)]
        self.observations.push(self.state);
    }
}

impl RunProgress {
    #[must_use]
    pub(crate) fn new(mode: ProgressMode, palette: Palette) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                mode,
                palette,
                state: RunProgressState::Idle,
                animation_ready: false,
                bar: None,
                #[cfg(test)]
                observations: vec![RunProgressState::Idle],
            })),
        }
    }

    pub(crate) async fn during<F>(&self, stage: RunStage, total: usize, future: F) -> F::Output
    where
        F: Future,
    {
        self.begin(stage, total);
        let mut future = std::pin::pin!(future);
        let interactive = self.lock().mode == ProgressMode::Interactive;
        let result = if interactive {
            tokio::select! {
                result = &mut future => result,
                () = tokio::time::sleep(ANIMATION_DELAY) => {
                    self.show_after_delay();
                    future.await
                }
            }
        } else {
            future.await
        };
        self.finish_stage();
        result
    }

    pub(crate) fn advance(&self) {
        let mut inner = self.lock();
        if matches!(inner.state, RunProgressState::Terminal(_)) {
            return;
        }
        let RunProgressState::Active {
            stage,
            amount: ProgressAmount::Known { completed, total },
        } = inner.state
        else {
            debug_assert!(false, "only known-total progress can advance");
            return;
        };
        let completed = completed.saturating_add(1).min(total);
        inner.state = RunProgressState::Active {
            stage,
            amount: ProgressAmount::Known { completed, total },
        };
        inner.record_observation();
        if inner.mode == ProgressMode::Plain {
            eprintln!("{} {completed}/{total}", stage.label());
        }
        if let Some(bar) = &inner.bar {
            bar.set_position(completed as u64);
        }
    }

    pub(crate) fn suspend_for_callout(&self) {
        let mut inner = self.lock();
        if matches!(inner.state, RunProgressState::Terminal(_)) {
            return;
        }
        inner.clear_bar();
        inner.state = RunProgressState::SuspendedForCallout;
        inner.record_observation();
    }

    pub(crate) fn resume_execution(&self) {
        let mut inner = self.lock();
        if matches!(inner.state, RunProgressState::Terminal(_)) {
            return;
        }
        inner.state = RunProgressState::Active {
            stage: RunStage::Execution,
            amount: ProgressAmount::Indeterminate,
        };
        inner.record_observation();
        if inner.mode == ProgressMode::Plain {
            eprintln!("{}", RunStage::Execution.label());
        } else if inner.mode == ProgressMode::Interactive && inner.animation_ready {
            inner.show_bar();
        }
    }

    pub(crate) fn terminal(&self, terminal: RunTerminalState) {
        let mut inner = self.lock();
        if matches!(inner.state, RunProgressState::Terminal(_)) {
            return;
        }
        inner.clear_bar();
        inner.animation_ready = false;
        inner.state = RunProgressState::Terminal(terminal);
        inner.record_observation();
    }

    fn begin(&self, stage: RunStage, total: usize) {
        let mut inner = self.lock();
        if matches!(inner.state, RunProgressState::Terminal(_)) {
            return;
        }
        inner.clear_bar();
        inner.animation_ready = false;
        inner.state = RunProgressState::Active {
            stage,
            amount: stage.initial(total),
        };
        inner.record_observation();
        if inner.mode == ProgressMode::Plain {
            match inner.state {
                RunProgressState::Active {
                    amount: ProgressAmount::Known { total, .. },
                    ..
                } => eprintln!("{} 0/{total}", stage.label()),
                RunProgressState::Active {
                    amount: ProgressAmount::Indeterminate,
                    ..
                } => eprintln!("{}", stage.label()),
                _ => unreachable!("stage was just activated"),
            }
        }
    }

    fn show_after_delay(&self) {
        let mut inner = self.lock();
        inner.animation_ready = true;
        if matches!(inner.state, RunProgressState::Active { .. }) {
            inner.show_bar();
        }
    }

    fn finish_stage(&self) {
        let mut inner = self.lock();
        inner.clear_bar();
        inner.animation_ready = false;
        if !matches!(inner.state, RunProgressState::Terminal(_)) {
            inner.state = RunProgressState::Idle;
            inner.record_observation();
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn observations(&self) -> Vec<RunProgressState> {
        self.lock().observations.clone()
    }
}

fn spinner_style(palette: Palette) -> ProgressStyle {
    let mut ticks = Dots::FRAMES
        .iter()
        .filter_map(|rows| rows.first().copied())
        .map(|frame| palette.cyan(frame))
        .collect::<Vec<_>>();
    ticks.push(String::new());
    let tick_refs = ticks.iter().map(String::as_str).collect::<Vec<_>>();
    ProgressStyle::with_template("{spinner} {msg}")
        .expect("static spinner template is valid")
        .tick_strings(&tick_refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_keep_animation_out_of_machine_and_nonterminal_runs() {
        assert_eq!(
            ProgressMode::for_run(true, false, true),
            ProgressMode::Interactive
        );
        assert_eq!(
            ProgressMode::for_run(false, false, true),
            ProgressMode::Plain
        );
        assert_eq!(
            ProgressMode::for_run(true, false, false),
            ProgressMode::Plain
        );
        assert_eq!(
            ProgressMode::for_run(true, true, true),
            ProgressMode::Hidden
        );
    }

    #[tokio::test]
    async fn typed_state_distinguishes_counts_waiting_callout_and_terminal_results() {
        let progress = RunProgress::new(ProgressMode::Hidden, Palette::plain());

        progress
            .during(RunStage::Connecting, 2, async {
                progress.advance();
                progress.advance();
            })
            .await;
        progress
            .during(RunStage::Execution, 0, async {
                progress.suspend_for_callout();
                progress.resume_execution();
            })
            .await;
        progress.terminal(RunTerminalState::Succeeded);

        let observations = progress.observations();
        assert!(observations.contains(&RunProgressState::Active {
            stage: RunStage::Connecting,
            amount: ProgressAmount::Known {
                completed: 2,
                total: 2,
            },
        }));
        assert!(observations.contains(&RunProgressState::Active {
            stage: RunStage::Execution,
            amount: ProgressAmount::Indeterminate,
        }));
        assert!(observations.contains(&RunProgressState::SuspendedForCallout));
        assert_eq!(
            observations.last(),
            Some(&RunProgressState::Terminal(RunTerminalState::Succeeded))
        );

        for terminal in [RunTerminalState::Failed, RunTerminalState::Cancelled] {
            let progress = RunProgress::new(ProgressMode::Hidden, Palette::plain());
            progress
                .during(RunStage::Execution, 0, std::future::ready(()))
                .await;
            progress.terminal(terminal);
            assert_eq!(
                progress.observations().last(),
                Some(&RunProgressState::Terminal(terminal))
            );
        }
    }

    #[tokio::test]
    async fn cancellation_is_absorbing_while_active_work_drains() {
        let progress = RunProgress::new(ProgressMode::Hidden, Palette::plain());
        let worker_progress = progress.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (drain_tx, drain_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            worker_progress
                .during(RunStage::Connecting, 1, async {
                    started_tx.send(()).expect("signal active work");
                    drain_rx.await.expect("release draining work");
                    worker_progress.advance();
                    worker_progress.suspend_for_callout();
                    worker_progress.resume_execution();
                })
                .await;
            worker_progress
                .during(RunStage::Verification, 1, async {
                    worker_progress.advance();
                })
                .await;
        });

        started_rx.await.expect("work became active");
        progress.terminal(RunTerminalState::Cancelled);
        drain_tx.send(()).expect("let active work drain");
        worker.await.expect("draining work completed");

        let observations = progress.observations();
        let cancelled = RunProgressState::Terminal(RunTerminalState::Cancelled);
        let terminal_index = observations
            .iter()
            .position(|state| *state == cancelled)
            .expect("cancellation was observed");
        assert!(
            observations[terminal_index..]
                .iter()
                .all(|state| *state == cancelled)
        );
        assert_eq!(observations.last(), Some(&cancelled));
    }
}
