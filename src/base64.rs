use base64ct::{Base64Unpadded, Encoding};

pub(crate) fn decode_base64(input: &str) -> crate::Result<Vec<u8>> {
    // Apple sometimes uses padded Base64 (e.g. Music App on iOS)
    // and sometimes removes the padding (e.g. Music App on macOS)
    // ¯\_(ツ)_/¯
    let actual_input = match input.find('=') {
        Some(index) => &input[..index],
        None => input,
    };

    let val = Base64Unpadded::decode_vec(actual_input)?;
    Ok(val)
}

pub(crate) fn encode_base64(input: &[u8]) -> String {
    Base64Unpadded::encode_string(input)
}
