//! The signing key: signs only payloads the checkers verified.
//!
//! Flows hold a [`SigningKey`], never the raw signer, and its only signing method takes a
//! [`Verified`], which only `crate::protocol` constructs after its checks. The signed payload is
//! the one the checks rebuilt; a signature is refused past the checks' `not_after`, and must
//! recover to the key's address.

use std::{fmt, sync::Arc};

use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, B256, eip191_hash_message};
use alloy_signer::Signer;
use chrono::{DateTime, Utc};

use super::checks::details::{self, differs, stale};
use crate::{
    error::{Error, SignerError, VerificationError},
    transport::Clock,
    types::SignatureBytes,
};

/// What a [`Verified`] asks the signer to sign.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Payload {
    /// EIP-712 typed data, rebuilt from checked terms and pinned domains.
    Typed(Box<TypedData>),
    /// An EIP-191 `personal_sign` text.
    Text(String),
}

/// A payload the checks verified for `signer`, signable until `not_after` (Unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verified {
    payload: Payload,
    digest: B256,
    signer: Address,
    not_after: Option<i64>,
}

impl Verified {
    /// Typed data the checks rebuilt; refused unless it hashes to the server's `server_digest`.
    pub(in crate::protocol) fn typed(
        rebuilt: TypedData,
        server_digest: B256,
        signer: Address,
        not_after: Option<i64>,
    ) -> Result<Self, VerificationError> {
        let digest = rebuilt.eip712_signing_hash().map_err(|_| differs(details::DIGEST))?;
        if digest != server_digest {
            return Err(differs(details::DIGEST));
        }
        Ok(Self { payload: Payload::Typed(Box::new(rebuilt)), digest, signer, not_after })
    }

    /// A text the checks rebuilt or matched exactly.
    pub(in crate::protocol) fn text(
        message: String,
        signer: Address,
        not_after: Option<i64>,
    ) -> Self {
        let digest = eip191_hash_message(message.as_bytes());
        Self { payload: Payload::Text(message), digest, signer, not_after }
    }

    /// The hash the signature signs: the EIP-712 digest or the EIP-191 hash.
    pub(crate) fn digest(&self) -> B256 {
        self.digest
    }

    /// The latest moment, in Unix seconds, the checks allow signing.
    #[cfg(test)]
    pub(crate) fn not_after(&self) -> Option<i64> {
        self.not_after
    }

    fn in_time(&self, now: DateTime<Utc>) -> bool {
        self.not_after.is_none_or(|limit| now.timestamp() <= limit)
    }
}

/// A signer that signs only [`Verified`] payloads.
#[derive(Clone)]
pub(crate) struct SigningKey {
    signer: Arc<dyn Signer + Send + Sync>,
    address: Address,
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey").field("address", &self.address).finish_non_exhaustive()
    }
}

impl SigningKey {
    /// Wraps `signer`, whose address is read once.
    pub(crate) fn new<S: Signer + Send + Sync + 'static>(signer: S) -> Self {
        let address = signer.address();
        Self { signer: Arc::new(signer), address }
    }

    /// The signer's address.
    pub(crate) fn address(&self) -> Address {
        self.address
    }

    /// Signs `verified`: refused unless it names this key and the clock is not past its
    /// `not_after`, checked again once the signer returns (a slow hardware or remote signer);
    /// the signature must recover to this key. Returns `r ‖ s ‖ v`, low `s`, `v` 27 or 28.
    pub(crate) async fn sign(
        &self,
        verified: &Verified,
        clock: &Clock,
    ) -> Result<SignatureBytes, Error> {
        if verified.signer != self.address {
            return Err(differs(details::SIGNER).into());
        }
        if !verified.in_time(clock()) {
            return Err(stale(details::SIGNING_WINDOW).into());
        }
        let signed = match &verified.payload {
            Payload::Typed(typed) => self.signer.sign_dynamic_typed_data(typed).await,
            Payload::Text(text) => self.signer.sign_message(text.as_bytes()).await,
        }
        .map_err(signer_failed)?;
        if !verified.in_time(clock()) {
            return Err(stale(details::SIGNING_WINDOW).into());
        }
        let signature = signed.normalize_s().unwrap_or(signed);
        let recovered = signature
            .recover_address_from_prehash(&verified.digest)
            .map_err(|error| signer_failed(error.into()))?;
        if recovered != self.address {
            return Err(Error::Signer(Box::new(SignerError::Mismatch {
                expected: self.address,
                recovered,
            })));
        }
        SignatureBytes::new(signature.as_bytes()).ok_or_else(|| {
            signer_failed(alloy_signer::Error::message("signature v is not 27 or 28"))
        })
    }
}

fn signer_failed(error: alloy_signer::Error) -> Error {
    Error::Signer(Box::new(SignerError::Failed(error)))
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        path::{Path, PathBuf},
        pin::Pin,
        sync::atomic::{AtomicI64, AtomicUsize, Ordering},
    };

    use alloy_primitives::{ChainId, Signature, U256, address};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::{
        error::VerificationKind,
        protocol::eip712::{self, Permit},
        testing::NOW,
    };

    /// A signer that counts its calls, may sign with another key than it claims, may take
    /// `delay` seconds of the test clock, and may return the high-`s` twin of its signature.
    struct Scripted {
        key: PrivateKeySigner,
        claims: Address,
        calls: Arc<AtomicUsize>,
        clock: Arc<AtomicI64>,
        delay: i64,
        high_s: bool,
    }

    impl Signer for Scripted {
        fn sign_hash<'life0, 'life1, 'async_trait>(
            &'life0 self,
            hash: &'life1 B256,
        ) -> Pin<Box<dyn Future<Output = alloy_signer::Result<Signature>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.clock.fetch_add(self.delay, Ordering::SeqCst);
                let low = self.key.sign_hash_sync(hash)?;
                if !self.high_s {
                    return Ok(low);
                }
                let order = U256::from_be_bytes(alloy_primitives::hex!(
                    "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
                ));
                Ok(Signature::new(low.r(), order - low.s(), !low.v()))
            })
        }

        fn address(&self) -> Address {
            self.claims
        }

        fn chain_id(&self) -> Option<ChainId> {
            None
        }

        fn set_chain_id(&mut self, _: Option<ChainId>) {}
    }

    struct Harness {
        key: SigningKey,
        address: Address,
        calls: Arc<AtomicUsize>,
        clock: Clock,
    }

    fn harness(other_key: bool, delay: i64, high_s: bool) -> Harness {
        let real = PrivateKeySigner::random();
        let address = real.address();
        let key = if other_key { PrivateKeySigner::random() } else { real };
        let calls = Arc::new(AtomicUsize::new(0));
        let time = Arc::new(AtomicI64::new(NOW));
        let signer = Scripted {
            key,
            claims: address,
            calls: calls.clone(),
            clock: time.clone(),
            delay,
            high_s,
        };
        let clock: Clock =
            Arc::new(move || DateTime::from_timestamp(time.load(Ordering::SeqCst), 0).unwrap());
        Harness { key: SigningKey::new(signer), address, calls, clock }
    }

    fn permit(owner: Address) -> TypedData {
        let message = Permit {
            owner,
            spender: address!("3100b7b49c0f9631c6af25877b5a4e13a47afad6"),
            value: U256::MAX,
            nonce: U256::ZERO,
            deadline: U256::from(NOW + 600),
        };
        eip712::typed(&message, eip712::domain("USDC", "2", 999, Some(Address::ZERO)))
    }

    fn typed(owner: Address, not_after: Option<i64>) -> Verified {
        let payload = permit(owner);
        let digest = payload.eip712_signing_hash().unwrap();
        Verified::typed(payload, digest, owner, not_after).unwrap()
    }

    fn refusal(error: Error) -> (VerificationKind, &'static str) {
        match error {
            Error::Verification(error) => (error.kind, error.detail),
            other => unreachable!("not a verification error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn signs_typed_data_and_text_recoverably_with_v_27_or_28() {
        let h = harness(false, 0, false);
        assert_eq!(h.key.address(), h.address);
        for verified in
            [typed(h.address, None), Verified::text("hello".to_owned(), h.address, None)]
        {
            let signature = h.key.sign(&verified, &h.clock).await.unwrap();
            let bytes = signature.as_bytes();
            assert!(matches!(bytes[64], 27 | 28));
            let parsed = Signature::from_raw(bytes).unwrap();
            assert_eq!(parsed.recover_address_from_prehash(&verified.digest()).unwrap(), h.address);
            assert!(parsed.normalize_s().is_none(), "s is low");
        }
        assert_eq!(h.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            Verified::text("hello".to_owned(), h.address, None).digest(),
            eip191_hash_message("hello")
        );
    }

    #[tokio::test]
    async fn refuses_a_payload_for_another_signer_without_calling_the_signer() {
        let h = harness(false, 0, false);
        let other = address!("1111111111111111111111111111111111111111");
        let error = h.key.sign(&typed(other, None), &h.clock).await.unwrap_err();
        assert_eq!(refusal(error), (VerificationKind::Differs, details::SIGNER));
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn refuses_past_not_after_without_calling_the_signer() {
        let h = harness(false, 0, false);
        let limit = NOW;
        assert!(h.key.sign(&typed(h.address, Some(limit)), &h.clock).await.is_ok());
        let error = h.key.sign(&typed(h.address, Some(limit - 1)), &h.clock).await.unwrap_err();
        assert_eq!(refusal(error), (VerificationKind::Stale, details::SIGNING_WINDOW));
        assert_eq!(h.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_signer_finishing_past_not_after_gives_no_signature() {
        let h = harness(false, 5, false);
        let limit = NOW + 4;
        let error = h.key.sign(&typed(h.address, Some(limit)), &h.clock).await.unwrap_err();
        assert_eq!(refusal(error), (VerificationKind::Stale, details::SIGNING_WINDOW));
        assert_eq!(h.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_signature_from_another_key_is_a_mismatch() {
        let h = harness(true, 0, false);
        let error = h.key.sign(&typed(h.address, None), &h.clock).await.unwrap_err();
        match error {
            Error::Signer(error) => match *error {
                SignerError::Mismatch { expected, recovered } => {
                    assert_eq!(expected, h.address);
                    assert_ne!(recovered, h.address);
                }
                SignerError::Failed(error) => unreachable!("{error}"),
            },
            other => unreachable!("{other:?}"),
        }
    }

    #[test]
    fn a_rebuilt_payload_must_hash_to_the_servers_digest() {
        let owner = address!("1111111111111111111111111111111111111111");
        let error = Verified::typed(permit(owner), B256::ZERO, owner, None).unwrap_err();
        assert_eq!((error.kind, error.detail), (VerificationKind::Differs, details::DIGEST));
    }

    /// A high-`s` signature is normalised (the server and the escrow require low `s`).
    #[tokio::test]
    async fn high_s_signatures_are_normalised() {
        let h = harness(false, 0, true);
        let verified = typed(h.address, None);
        let signature = h.key.sign(&verified, &h.clock).await.unwrap();
        let parsed = Signature::from_raw(signature.as_bytes()).unwrap();
        assert!(parsed.normalize_s().is_none());
        assert_eq!(parsed.recover_address_from_prehash(&verified.digest()).unwrap(), h.address);
    }

    /// Only this module calls a signer's raw signing methods.
    #[test]
    fn raw_signer_calls_live_only_in_the_signing_module() {
        fn visit(dir: &Path, offenders: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, offenders);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && !path.ends_with("protocol/signing.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let calls = [
                        ".sign_hash(",
                        ".sign_message(",
                        ".sign_dynamic_typed_data(",
                        ".sign_typed_data(",
                        "sign_hash_sync(",
                        "sign_message_sync(",
                        "sign_typed_data_sync(",
                        "sign_dynamic_typed_data_sync(",
                    ];
                    if calls.iter().any(|call| text.contains(call)) {
                        offenders.push(path);
                    }
                }
            }
        }
        let mut offenders = Vec::new();
        visit(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut offenders);
        assert!(
            offenders.is_empty(),
            "raw signer calls outside protocol/signing.rs: {offenders:?}"
        );
    }
}
