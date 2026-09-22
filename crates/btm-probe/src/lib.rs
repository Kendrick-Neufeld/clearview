//! Raw acquisition layer: everything that touches `/proc`, `/sys` or cgroups lives here.
//!
//! This crate deliberately knows nothing about apps, grouping or presentation. It
//! reports what the kernel says, as faithfully as possible, and leaves interpretation
//! to `btm-model`. The only cleverness here is arithmetic that cannot be done without
//! two samples (rates) and the guards needed to keep that arithmetic honest.

pub mod conf;
pub mod desktop;
pub mod process;
pub mod sampler;
pub mod system;
pub mod wm;

pub use sampler::{ProcSample, Sample, Sampler};
