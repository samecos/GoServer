//! Runtime-selected CPU search kernels. No global target-cpu requirement.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchSimd {
    #[default]
    Auto,
    Scalar,
    Avx512,
}

impl std::str::FromStr for SearchSimd {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "scalar" => Ok(Self::Scalar),
            "avx512" => Ok(Self::Avx512),
            _ => Err("search SIMD must be auto, scalar or avx512"),
        }
    }
}

impl SearchSimd {
    pub fn resolve(self) -> Result<Self, &'static str> {
        #[cfg(target_arch = "x86_64")]
        let avx512 = std::is_x86_feature_detected!("avx512f");
        #[cfg(not(target_arch = "x86_64"))]
        let avx512 = false;
        Self::resolve_features(self, avx512)
    }

    fn resolve_features(self, avx512: bool) -> Result<Self, &'static str> {
        match self {
            Self::Auto => Ok(if avx512 { Self::Avx512 } else { Self::Scalar }),
            Self::Scalar => Ok(Self::Scalar),
            Self::Avx512 if avx512 => Ok(Self::Avx512),
            _ => Err("requested search SIMD is unavailable on this CPU/OS"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_instructions_fall_back_only_in_auto_mode() {
        assert_eq!(
            SearchSimd::Auto.resolve_features(false),
            Ok(SearchSimd::Scalar)
        );
        assert!(SearchSimd::Avx512.resolve_features(false).is_err());
        assert_eq!(
            SearchSimd::Auto.resolve_features(true),
            Ok(SearchSimd::Avx512)
        );
        assert!("AVX".parse::<SearchSimd>().is_err());
        assert!("avx2".parse::<SearchSimd>().is_err());
    }
}
