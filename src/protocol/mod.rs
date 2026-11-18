//! Check before sign: pure, I/O-free checks of every server payload against the request and the
//! pinned deployments, and the signing key that signs only what they verified.

pub(crate) mod aster;
pub(crate) mod checks;
pub(crate) mod eip712;
pub(crate) mod fees;
pub(crate) mod gmx;
pub(crate) mod hyperliquid;
pub(crate) mod lighter;
pub(crate) mod polymarket;
pub(crate) mod relay;
pub(crate) mod signing;
pub(crate) mod siwe;
