//! TLS: one rustls configuration with an explicit crypto provider and the platform verifier,
//! shared by HTTP and the event WebSocket. The process-wide default provider is never touched.

#[cfg(not(any(feature = "aws-lc-rs", feature = "ring")))]
compile_error!("enable feature `aws-lc-rs` or `ring`");

use std::sync::Arc;

use rustls::{ClientConfig, crypto::CryptoProvider};
use rustls_platform_verifier::BuilderVerifierExt as _;

use crate::{Error, Result};

/// The HTTP configuration (ALPN `h2`, `http/1.1`) and the WebSocket one (ALPN `http/1.1`).
pub(crate) fn configs() -> Result<(ClientConfig, Arc<ClientConfig>)> {
    let config_error = |error: rustls::Error| Error::Config(format!("tls: {error}").into());
    let base = ClientConfig::builder_with_provider(Arc::new(provider()))
        .with_safe_default_protocol_versions()
        .map_err(config_error)?
        .with_platform_verifier()
        .map_err(config_error)?
        .with_no_client_auth();
    let mut ws = base.clone();
    ws.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut http = base;
    http.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok((http, Arc::new(ws)))
}

/// aws-lc-rs when enabled (also when `ring` is enabled too), else ring.
#[cfg(feature = "aws-lc-rs")]
fn provider() -> CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

#[cfg(all(feature = "ring", not(feature = "aws-lc-rs")))]
fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_set_alpn_and_leave_the_process_default_alone() {
        let (http, ws) = configs().unwrap();
        assert_eq!(http.alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
        assert_eq!(ws.alpn_protocols, [b"http/1.1".to_vec()]);
        assert!(CryptoProvider::get_default().is_none());
    }
}
