//! CPU-only Chinese Go rules and asynchronous Monte-Carlo Graph Search.
//! Reference: KataGo 231e1c4b938f068628a5e3e59a3e842ad5fc92cd.
//! See `REFERENCE.md` for the supported parameter profile and parity limits.
mod position;
#[doc(hidden)]
pub mod profiling;
mod search;
mod simd;
mod value;

pub use position::{Color, Move, Position, PositionError, Terminal};
pub use search::{
    Candidate, Completion, EvalToken, EvaluationRequest, Search, SearchConfig, SearchError,
    SearchSnapshot, SearchStep,
};
pub use simd::SearchSimd;
pub use value::{Evaluation, NodeStats};
