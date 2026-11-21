//! Terminal visualization rendering derived from real waveform samples.

use flux_app::waveform_sample;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VizMode {
    Waveform,
    Bars,
    Pulse,
}

impl VizMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Waveform => "Waveform",
            Self::Bars => "Bars",
            Self::Pulse => "Pulse",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Waveform => Self::Bars,
            Self::Bars => Self::Pulse,
            Self::Pulse => Self::Waveform,
        }
    }
}

#[allow(clippy::needless_range_loop)]
pub(crate) fn visualization_canvas(
    samples: &[f32],
    mode: VizMode,
    gain: f32,
    width: usize,
    height: usize,
) -> Vec<String> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let center = height / 2;
    let mut canvas = vec![vec![' '; width]; height];
    match mode {
        VizMode::Waveform => {
            let mut previous = center;
            for column in 0..width {
                let t = if width <= 1 {
                    0.0
                } else {
                    column as f32 / (width - 1) as f32
                };
                let value = (waveform_sample(samples, t) * gain).clamp(-1.0, 1.0);
                let amplitude =
                    (value.abs() * (height.saturating_sub(1)) as f32 / 2.0).round() as usize;
                let position = if value >= 0.0 {
                    center.saturating_sub(amplitude)
                } else {
                    center + amplitude
                }
                .min(height - 1);
                let low = previous.min(position);
                let high = previous.max(position);
                for row in low..=high {
                    canvas[row][column] = '•';
                }
                previous = position;
            }
        }
        VizMode::Bars => {
            const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
            for column in 0..width {
                let t = if width <= 1 {
                    0.0
                } else {
                    column as f32 / (width - 1) as f32
                };
                let value = (waveform_sample(samples, t) * gain).abs().clamp(0.0, 1.0);
                let level = ((value * 7.0).round() as usize).min(7);
                for row in 0..height {
                    canvas[row][column] = LEVELS[level];
                }
            }
        }
        VizMode::Pulse => {
            for column in 0..width {
                let t = if width <= 1 {
                    0.0
                } else {
                    column as f32 / (width - 1) as f32
                };
                let value = (waveform_sample(samples, t) * gain).abs().clamp(0.0, 1.0);
                let reach = (value * center as f32).round() as usize;
                for offset in 0..=reach {
                    if center >= offset {
                        canvas[center - offset][column] = '█';
                    }
                    if center + offset < height {
                        canvas[center + offset][column] = '█';
                    }
                }
            }
        }
    }
    canvas
        .into_iter()
        .map(|line| line.into_iter().collect::<String>())
        .collect()
}
