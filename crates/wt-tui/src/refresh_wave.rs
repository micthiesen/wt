//! A bounded, idle-free port of the TypeScript title-bar traveling wave.
use std::time::Duration;

pub(crate) const WIDTH: usize = 12;
pub(crate) const PERIOD: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub(crate) struct RefreshWave {
    width: f32,
    amplitude: f32,
    phase: usize,
}

impl RefreshWave {
    pub(crate) fn active(&self, count: usize) -> bool {
        count > 0 || self.width > 0.0 || self.amplitude > 0.0
    }

    pub(crate) fn tick(&mut self, count: usize) {
        self.width = ease(self.width, count.min(WIDTH) as f32, 0.7, 0.45, 0.5);
        self.amplitude = ease(
            self.amplitude,
            if self.width > 0.0 { 1.0 } else { 0.0 },
            0.7,
            0.7,
            0.02,
        );
        self.phase = if self.active(0) {
            (self.phase + 1) % 14
        } else {
            0
        };
    }

    /// Always occupies the same cells, including while idle and draining.
    pub(crate) fn text(&self) -> String {
        const HEIGHTS: [u32; 14] = [1, 2, 3, 4, 5, 6, 7, 8, 7, 6, 5, 4, 3, 2];
        (0..WIDTH)
            .map(|i| {
                let height = (HEIGHTS[(self.phase + i) % HEIGHTS.len()] as f32 * self.amplitude)
                    .round() as u32;
                if i >= self.width.round() as usize || height == 0 {
                    ' '
                } else {
                    char::from_u32(0x2580 + height).unwrap_or(' ')
                }
            })
            .collect()
    }
}

fn ease(current: f32, target: f32, grow: f32, shrink: f32, snap: f32) -> f32 {
    let difference = target - current;
    if difference.abs() < snap {
        target
    } else {
        current + difference * if difference > 0.0 { grow } else { shrink }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wave_is_bounded_and_stops_completely_after_drain() {
        let mut wave = RefreshWave::default();
        assert!(!wave.active(0));
        assert_eq!(wave.text(), " ".repeat(WIDTH));
        for _ in 0..100 {
            wave.tick(usize::MAX);
            assert_eq!(wave.text().width(), WIDTH);
        }
        let first = wave.text();
        wave.tick(usize::MAX);
        assert_ne!(first, wave.text());
        for _ in 0..30 {
            wave.tick(0);
        }
        assert!(
            !wave.active(0),
            "drain must terminate rather than wake forever"
        );
        assert_eq!(wave.text(), " ".repeat(WIDTH));
    }

    #[test]
    fn refreshing_does_not_move_header_text_or_touch_body() {
        use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
        use wt_core::Domain;
        use wt_runtime::{PendingPhase, PendingView, SourceSnapshot, SourceState, Target};

        let board = crate::Board {
            pending_views: vec![PendingView {
                target: Target::board(None),
                label: "creating checkout".into(),
                phase: PendingPhase::InFlight,
            }],
            ..Default::default()
        };
        let mut model = crate::Model::default();
        model.apply(SourceSnapshot::test_live(
            Some(board.clone()),
            SourceState::Ready,
            Domain::Git,
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        let mut retained = Buffer::empty(Rect::new(0, 0, 100, 10));
        retained[(3, 5)].set_symbol("界");
        let mut draw = |model: &crate::Model| {
            terminal
                .draw(|frame| {
                    frame.buffer_mut().clone_from(&retained);
                    crate::render::refresh_header(frame, model);
                })
                .unwrap();
            assert_eq!(terminal.backend().buffer()[(3, 5)].symbol(), "界");
            (0..100)
                .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                .collect::<String>()
        };
        let idle = draw(&model);
        model.apply(SourceSnapshot::test_live(
            Some(board),
            SourceState::Refreshing,
            Domain::Git,
        ));
        assert_eq!(crate::render::refreshing_count(&model), 1);
        model.refresh_wave.tick(1);
        let refreshing = draw(&model);
        assert_eq!(
            idle[..idle.find("creating checkout").unwrap()].width(),
            refreshing[..refreshing.find("creating checkout").unwrap()].width()
        );
        assert!(idle.contains("creating checkout"));
        assert_ne!(idle, refreshing);
        model.help = true;
        assert!(!crate::render::wave_visible(&model));
    }

    #[test]
    fn wave_counts_remote_lanes_but_not_optional_perf_sampling() {
        use wt_core::Domain;
        use wt_runtime::{SourceSnapshot, SourceState};
        let mut board = crate::Board::default();
        let remote =
            SourceSnapshot::<()>::test_live(None, SourceState::Refreshing, Domain::Sessions);
        board
            .host_domains
            .insert("MaxBook".into(), remote.domains().clone());
        let mut model = crate::Model::default();
        model.apply(SourceSnapshot::test_live(
            Some(board.clone()),
            SourceState::Refreshing,
            Domain::Git,
        ));
        assert_eq!(crate::render::refreshing_count(&model), 2);
        model.apply(SourceSnapshot::test_live(
            Some(board),
            SourceState::Refreshing,
            Domain::Perf,
        ));
        assert_eq!(crate::render::refreshing_count(&model), 1);
    }
}
