use alloy::primitives::U256;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum AmountError {
    #[error("lossy truncation")]
    LossyTruncation,
    #[error("overflow")]
    Overflow,
}

pub fn validate_amount(amount: U256, decimals: u8, decimals_out: u8) -> Result<u32, AmountError> {
    assert!(
        decimals >= decimals_out,
        "amount decimals are less than expected in result, scaling up is not supported"
    );
    let amount_scaled = if decimals == decimals_out {
        amount
    } else {
        let downscale_factor = U256::from(10).pow(U256::from(decimals - decimals_out));
        let (quotient, remainder) = amount.div_rem(downscale_factor);
        if !remainder.is_zero() {
            return Err(AmountError::LossyTruncation);
        }
        quotient
    };
    u32::try_from(amount_scaled).map_err(|_| AmountError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Add;
    use std::ops::Mul;

    #[test]
    fn test_validate_amount() {
        const DECIMALS: u8 = 18;
        const DECIMALS_OUT: u8 = 8;
        let eth_wei = U256::from(10).pow(U256::from(DECIMALS));
        let gwei = U256::from(10).pow(U256::from(9));

        assert_eq!(validate_amount(U256::from(123), 0, 0), Ok(123));
        assert_eq!(validate_amount(U256::from(0), DECIMALS, DECIMALS_OUT), Ok(0));
        assert_eq!(
            validate_amount(U256::from(123), DECIMALS, DECIMALS_OUT),
            Err(AmountError::LossyTruncation)
        );
        assert_eq!(validate_amount(U256::from(1230).mul(gwei), DECIMALS, DECIMALS_OUT), Ok(123));
        assert_eq!(
            validate_amount(U256::from(42).mul(eth_wei), DECIMALS, DECIMALS_OUT),
            Ok(4200000000)
        );
        assert_eq!(
            validate_amount(U256::from(42).mul(eth_wei).add(U256::from(1)), DECIMALS, DECIMALS_OUT),
            Err(AmountError::LossyTruncation)
        );
        assert_eq!(
            validate_amount(U256::from(43).mul(eth_wei), DECIMALS, DECIMALS_OUT),
            Err(AmountError::Overflow)
        );
        assert_eq!(
            validate_amount(U256::from(1).mul(eth_wei), DECIMALS, DECIMALS),
            Err(AmountError::Overflow)
        );
        assert_eq!(
            validate_amount(U256::MAX, DECIMALS, DECIMALS_OUT),
            Err(AmountError::LossyTruncation)
        );
    }
}
