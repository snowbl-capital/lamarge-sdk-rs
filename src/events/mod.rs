//! The event feed as a `Stream`, and state tracking over it.

mod socket;
mod stream;
#[cfg(test)]
mod tests;
mod tracker;

pub use stream::*;
pub use tracker::*;
