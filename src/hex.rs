use alloy::primitives::U64;
use hex::FromHexError;
use std::str::FromStr;

pub fn hex_decode_prefixed(input: &str) -> anyhow::Result<Vec<u8>, FromHexError> {
    hex::decode(input.strip_prefix("0x").unwrap_or(input))
}

pub fn hex_decode_u64(input: &str) -> anyhow::Result<u64> {
    let num = U64::from_str(input)?;
    Ok(num.to())
}
