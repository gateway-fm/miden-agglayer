use hex::FromHexError;

pub fn hex_decode_prefixed(input: &str) -> anyhow::Result<Vec<u8>, FromHexError> {
    hex::decode(input.strip_prefix("0x").unwrap_or(input))
}
