use malak_layers::StoreLocation;
use minicbor::{Decoder, Encoder};

pub(super) fn encode(output: &mut Encoder<Vec<u8>>, location: &StoreLocation) {
    output
        .array(2)
        .unwrap()
        .bytes(&location.filesystem_id)
        .unwrap()
        .u64(location.folder_number)
        .unwrap();
}

pub(super) fn decode(input: &mut Decoder<'_>) -> Option<StoreLocation> {
    if input.array().ok()? != Some(2) {
        return None;
    }
    Some(StoreLocation {
        filesystem_id: input.bytes().ok()?.to_vec(),
        folder_number: input.u64().ok()?,
    })
}
