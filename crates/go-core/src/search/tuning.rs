use super::SearchError;
use crate::Color;
use serde::{Deserialize, Serialize};

/// The reference side is fixed for the entire search, including opponent leaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PdaPlayer {
    #[default]
    Root,
    Black,
    White,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct SearchTuning {
    pub playout_doubling_advantage: f64,
    pub playout_doubling_advantage_pla: PdaPlayer,
    pub wide_root_noise: f64,
}

impl SearchTuning {
    pub fn validate(&self) -> Result<(), SearchError> {
        if !self.playout_doubling_advantage.is_finite()
            || !(-3.0..=3.0).contains(&self.playout_doubling_advantage)
        {
            return Err(SearchError::Config(
                "playoutDoublingAdvantage must be between -3 and 3",
            ));
        }
        if !self.wide_root_noise.is_finite() || !(0.0..=5.0).contains(&self.wide_root_noise) {
            return Err(SearchError::Config("wideRootNoise must be between 0 and 5"));
        }
        Ok(())
    }

    pub fn effective_pda(&self, root: Color, next: Color) -> f64 {
        let reference = match self.playout_doubling_advantage_pla {
            PdaPlayer::Root => root,
            PdaPlayer::Black => Color::Black,
            PdaPlayer::White => Color::White,
        };
        if self.playout_doubling_advantage == 0.0 {
            0.0
        } else if next == reference {
            self.playout_doubling_advantage
        } else {
            -self.playout_doubling_advantage
        }
    }
}

/// Reproducible SplitMix64 stream; used only for optional root exploration.
pub(super) struct RootNoise(u64);

impl RootNoise {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn uniform(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        // Strictly inside (0,1), so Box-Muller never takes log(0).
        (((z ^ (z >> 31)) >> 12) as f64 + 0.5) / 4503599627370496.0
    }

    pub(super) fn bonus(&mut self, strength: f64) -> f64 {
        if self.uniform() >= 0.5 {
            return 0.0;
        }
        let gaussian =
            (-2.0 * self.uniform().ln()).sqrt() * (std::f64::consts::TAU * self.uniform()).cos();
        strength * gaussian.abs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_bonus_has_half_zero_half_normal_distribution() {
        let mut rng = RootNoise::new(123);
        let samples: Vec<_> = (0..100_000).map(|_| rng.bonus(1.0)).collect();
        let zeros = samples.iter().filter(|&&x| x == 0.0).count();
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let square = samples.iter().map(|x| x * x).sum::<f64>() / samples.len() as f64;
        assert!((49_000..51_000).contains(&zeros));
        assert!((mean - 0.5 * (2.0 / std::f64::consts::PI).sqrt()).abs() < 0.01);
        assert!((square - 0.5).abs() < 0.015);
        assert!(samples.iter().all(|x| x.is_finite() && *x >= 0.0));
    }

    #[test]
    fn validation_rejects_out_of_range_and_nonfinite_parameters() {
        for pda in [-3.01, 3.01, f64::NAN, f64::INFINITY] {
            assert!(SearchTuning {
                playout_doubling_advantage: pda,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for wide in [-0.01, 5.01, f64::NAN, f64::INFINITY] {
            assert!(SearchTuning {
                wide_root_noise: wide,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for pda in [-3.0, 0.0, 3.0] {
            for wide in [0.0, 5.0] {
                assert!(SearchTuning {
                    playout_doubling_advantage: pda,
                    wide_root_noise: wide,
                    ..Default::default()
                }
                .validate()
                .is_ok());
            }
        }
    }
}
