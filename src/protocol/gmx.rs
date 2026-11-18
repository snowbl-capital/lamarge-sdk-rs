//! GMX's own withdrawal: funds leave a GMX Account only by its owner's `transferOut` on GMX's
//! `MultichainTransferRouter`, which LaMarge never relays. The SDK rebuilds the call from the
//! amount alone, never from calldata the API supplies.

use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolCall;

use super::relay::GMX_ROUTER;
use crate::deployments;

alloy_sol_types::sol! {
    /// GMX's `BridgeOutParams`.
    struct BridgeOutParams {
        address token;
        uint256 amount;
        uint256 minAmountOut;
        address provider;
        bytes data;
    }

    function transferOut(BridgeOutParams params);
}

/// GMX's `MultichainTransferRouter` on Arbitrum.
pub(crate) const TRANSFER_ROUTER: alloy_primitives::Address = GMX_ROUTER;

/// `transferOut((token, amount, 0, 0x0, 0x))`: `amount` of Arbitrum USDC from the sender's GMX
/// Account to the sender's own address, never elsewhere.
pub(crate) fn transfer_out(amount: U256) -> Bytes {
    let params = BridgeOutParams {
        token: deployments::GMX.token,
        amount,
        minAmountOut: U256::ZERO,
        provider: alloy_primitives::Address::ZERO,
        data: Bytes::new(),
    };
    transferOutCall { params }.abi_encode().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_out_equals_the_typescript_sdks_calldata() {
        let word = |value: U256| format!("{value:064x}");
        let token = U256::from_be_slice(deployments::GMX.token.as_slice());
        let amount = U256::from(123_456_789u64);
        let expected = format!(
            "0x0b9066d9{}{}{}{}{}{}{}",
            word(U256::from(32u8)),
            word(token),
            word(amount),
            word(U256::ZERO),
            word(U256::ZERO),
            word(U256::from(160u8)),
            word(U256::ZERO),
        );
        assert_eq!(transfer_out(amount).to_string(), expected);
    }
}
