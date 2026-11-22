use std::time::{Duration, Instant};

use crate::settings::{TimerMode, TimerSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerState {
    Ready,
    Running,
    Paused,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerSnapshot {
    pub state: TimerState,
    pub seconds: u64,
}

impl TimerSnapshot {
    pub fn formatted(self) -> String {
        let hours = self.seconds / 3600;
        let minutes = self.seconds / 60 % 60;
        let seconds = self.seconds % 60;
        if hours == 0 {
            format!("{minutes:02}:{seconds:02}")
        } else {
            format!("{hours}:{minutes:02}:{seconds:02}")
        }
    }
}

pub struct Timer {
    settings: TimerSettings,
    elapsed: Duration,
    running_since: Option<Instant>,
    started: bool,
    presenting: bool,
    auto_start: bool,
}

impl Timer {
    pub fn new(settings: TimerSettings) -> Self {
        Self {
            settings: settings.normalized(),
            elapsed: Duration::ZERO,
            running_since: None,
            started: false,
            presenting: false,
            auto_start: false,
        }
    }

    pub fn set_settings(&mut self, settings: TimerSettings) {
        let settings = settings.normalized();
        if self.settings != settings {
            self.settings = settings;
            self.reset();
        }
    }

    pub fn is_presenting(&self) -> bool {
        self.presenting
    }

    pub fn begin_presentation(&mut self) {
        if !self.presenting {
            self.presenting = true;
            self.auto_start = true;
        }
    }

    pub fn end_presentation(&mut self, now: Instant) {
        self.pause(now);
        self.presenting = false;
        self.auto_start = false;
    }

    pub fn page_changed(&mut self, now: Instant) {
        if self.auto_start {
            self.start(now);
        }
    }

    pub fn start(&mut self, now: Instant) {
        if !self.presenting {
            return;
        }
        self.auto_start = false;
        if self.running_since.is_none() && self.snapshot(now).state != TimerState::Expired {
            self.running_since = Some(now);
            self.started = true;
        }
    }

    pub fn pause(&mut self, now: Instant) {
        self.elapsed = self.elapsed_at(now);
        self.running_since = None;
    }

    pub fn reset(&mut self) {
        self.elapsed = Duration::ZERO;
        self.running_since = None;
        self.started = false;
        self.auto_start = false;
    }

    pub fn snapshot(&self, now: Instant) -> TimerSnapshot {
        let elapsed = self.elapsed_at(now);
        let state = if self.running_since.is_some() {
            TimerState::Running
        } else if self.started {
            TimerState::Paused
        } else {
            TimerState::Ready
        };

        match self.settings.mode {
            TimerMode::CountUp => TimerSnapshot {
                state,
                seconds: elapsed.as_secs(),
            },
            TimerMode::CountDown => {
                let initial = Duration::from_secs(self.settings.countdown_seconds.into());
                let remaining = initial.saturating_sub(elapsed);
                TimerSnapshot {
                    state: if remaining.is_zero() {
                        TimerState::Expired
                    } else {
                        state
                    },
                    seconds: remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0),
                }
            }
        }
    }

    fn elapsed_at(&self, now: Instant) -> Duration {
        self.running_since.map_or(self.elapsed, |since| {
            self.elapsed
                .saturating_add(now.saturating_duration_since(since))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_page_move_starts_and_reprojection_resumes() {
        let mut timer = Timer::new(TimerSettings::default());
        let now = Instant::now();
        let at = |seconds| now + Duration::from_secs(seconds);

        timer.page_changed(at(2));
        timer.start(at(3));
        assert_eq!(timer.snapshot(at(4)).state, TimerState::Ready);

        timer.begin_presentation();
        assert!(timer.is_presenting());
        assert_eq!(timer.snapshot(at(5)).seconds, 0);
        timer.page_changed(at(10));
        timer.page_changed(at(11));
        assert_eq!(timer.snapshot(at(13)).seconds, 3);

        timer.end_presentation(at(14));
        let paused = timer.snapshot(at(30));
        assert!(!timer.is_presenting());
        assert_eq!(paused.state, TimerState::Paused);
        assert_eq!(paused.seconds, 4);

        timer.begin_presentation();
        assert_eq!(timer.snapshot(at(31)), paused);
        timer.page_changed(at(32));
        assert_eq!(timer.snapshot(at(35)).seconds, 7);
    }

    #[test]
    fn manual_start_and_reset_consume_automatic_start() {
        let mut timer = Timer::new(TimerSettings::default());
        let now = Instant::now();
        let at = |seconds| now + Duration::from_secs(seconds);

        timer.begin_presentation();
        timer.start(at(1));
        timer.pause(at(3));
        timer.page_changed(at(4));
        assert_eq!(timer.snapshot(at(5)).seconds, 2);
        assert_eq!(timer.snapshot(at(5)).state, TimerState::Paused);

        timer.start(at(6));
        assert_eq!(timer.snapshot(at(9)).seconds, 5);
        timer.reset();
        timer.page_changed(at(10));
        assert_eq!(timer.snapshot(at(11)).state, TimerState::Ready);
        assert_eq!(timer.snapshot(at(11)).seconds, 0);
    }

    #[test]
    fn reset_before_first_move_requires_manual_start() {
        let mut timer = Timer::new(TimerSettings::default());
        let now = Instant::now();
        timer.begin_presentation();
        timer.reset();
        timer.page_changed(now);
        assert_eq!(timer.snapshot(now).state, TimerState::Ready);
        timer.start(now);
        assert_eq!(timer.snapshot(now).state, TimerState::Running);
    }

    #[test]
    fn countdown_holds_zero_until_reset() {
        let mut timer = Timer::new(TimerSettings {
            mode: TimerMode::CountDown,
            countdown_seconds: 2,
        });
        let now = Instant::now();
        let at = |milliseconds| now + Duration::from_millis(milliseconds);

        timer.begin_presentation();
        timer.start(now);
        assert_eq!(timer.snapshot(at(999)).formatted(), "00:02");
        assert_eq!(timer.snapshot(at(1999)).formatted(), "00:01");
        assert_eq!(timer.snapshot(at(2000)).state, TimerState::Expired);
        assert_eq!(timer.snapshot(at(2000)).formatted(), "00:00");
        timer.end_presentation(at(3000));
        timer.begin_presentation();
        timer.page_changed(at(4000));
        assert_eq!(timer.snapshot(at(5000)).state, TimerState::Expired);
        timer.reset();
        assert_eq!(timer.snapshot(at(5000)).formatted(), "00:02");
    }

    #[test]
    fn changed_configuration_resets_the_running_timer() {
        let now = Instant::now();
        let mut timer = Timer::new(TimerSettings::default());
        timer.begin_presentation();
        timer.page_changed(now);
        timer.set_settings(TimerSettings::default());
        assert_eq!(timer.snapshot(now).state, TimerState::Running);

        timer.set_settings(TimerSettings {
            mode: TimerMode::CountDown,
            countdown_seconds: 3601,
        });
        assert_eq!(timer.snapshot(now).state, TimerState::Ready);
        assert_eq!(timer.snapshot(now).formatted(), "1:00:01");
    }
}
