use super::path::read_bytes;
use super::{HandoverTarget, HandoverWrite, local_address};
use crate::Error;
use malak_proto::KeyPair;
use minicbor::{Decoder, Encoder};
use std::path::Path;
use zeroize::Zeroizing;

const VERSION: u8 = 1;

/// Local credentials for the owner door, stored inside the private state folder.
pub struct OwnerCredential {
    address: String,
    pinned_board: [u8; 32],
    key: Zeroizing<[u8; 32]>,
}

impl OwnerCredential {
    /// The credential's file name in the board state directory.
    pub const FILE_NAME: &str = "owner.credential";

    /// Read the owner credential from the board's private state folder.
    pub fn read(state: impl AsRef<Path>) -> Result<Self, Error> {
        let bytes = read_bytes(&state.as_ref().join(Self::FILE_NAME))?;
        decode(&bytes).ok_or_else(|| Error::refused("invalid owner credential"))
    }

    /// Listen address of the owner door.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The board's pinned handshake key.
    pub const fn board_key(&self) -> &[u8; 32] {
        &self.pinned_board
    }

    /// Borrow the key for a Noise opening at the owner door.
    pub fn private_key(&self) -> &[u8; 32] {
        &self.key
    }

    pub(crate) fn prepare(state: &Path, uid: u32) -> Result<HandoverTarget, Error> {
        HandoverTarget::prepare(state.join(Self::FILE_NAME), uid, HandoverWrite::New)
    }

    pub(crate) fn prepare_resume(
        state: &Path,
        uid: u32,
        address: &str,
        board: &KeyPair,
        owner: &KeyPair,
    ) -> Result<HandoverTarget, Error> {
        HandoverTarget::prepare_resume(&state.join(Self::FILE_NAME), uid, &|bytes| {
            decode(bytes).is_some_and(|credential| {
                credential.address == address
                    && credential.board_key() == board.public()
                    && credential.private_key() == owner.private()
            })
        })
    }

    pub(crate) fn save(
        path: HandoverTarget,
        address: &str,
        board: &KeyPair,
        owner: &KeyPair,
    ) -> Result<(), Error> {
        let mut encoder = Encoder::new(Vec::new());
        encoder
            .array(4)
            .unwrap()
            .u8(VERSION)
            .unwrap()
            .str(address)
            .unwrap()
            .bytes(board.public())
            .unwrap()
            .bytes(owner.private())
            .unwrap();
        let bytes = Zeroizing::new(encoder.into_writer());
        path.save(&bytes)?;
        Ok(())
    }
}

fn decode(bytes: &[u8]) -> Option<OwnerCredential> {
    let mut input = Decoder::new(bytes);
    if input.array().ok()? != Some(4) || input.u8().ok()? != VERSION {
        return None;
    }
    let address = input.str().ok()?.to_owned();
    let pinned_board = input.bytes().ok()?.try_into().ok()?;
    let key = Zeroizing::new(input.bytes().ok()?.try_into().ok()?);
    (input.position() == bytes.len() && local_address(&address)).then_some(OwnerCredential {
        address,
        pinned_board,
        key,
    })
}
