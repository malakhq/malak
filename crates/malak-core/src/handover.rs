use crate::{Error, ParticipantId};
use malak_proto::{Binding, Entrance};
use minicbor::{Decoder, Encoder};
use zeroize::Zeroizing;

mod owner;
mod path;
pub use owner::OwnerCredential;
pub use path::{HandoverTarget, HandoverWrite};

const VERSION: u8 = 1;

/// The local file a participant uses to authenticate to its board.
///
/// The private key and mechanism bytes are cleared when this value is dropped.
pub struct HandoverFile {
    binding: Binding,
    address: String,
    board_key: [u8; 32],
    id: ParticipantId,
    mechanism: String,
    data: Zeroizing<Vec<u8>>,
    key: Zeroizing<[u8; 32]>,
}

impl HandoverFile {
    /// Prepare credentials for a general participant or the lead.
    ///
    /// The lead uses an empty mechanism name and empty mechanism data.
    pub fn new(
        binding: Binding,
        address: String,
        board_key: [u8; 32],
        id: ParticipantId,
        mechanism: String,
        data: Zeroizing<Vec<u8>>,
        key: Zeroizing<[u8; 32]>,
    ) -> Result<Self, Error> {
        let valid_role = match binding.entrance {
            Entrance::Lead => id.is_lead() && mechanism.is_empty() && data.is_empty(),
            Entrance::General => !id.is_lead() && !mechanism.is_empty(),
            Entrance::Owner => false,
        };
        let valid_address = local_address(&address)
            || (binding.entrance == Entrance::General && guest_address(&address));
        if !valid_role || binding.version == 0 || !valid_address {
            return Err(Error::refused("invalid local handover"));
        }
        Ok(Self {
            binding,
            address,
            board_key,
            id,
            mechanism,
            data,
            key,
        })
    }

    /// Load private credentials from a local file.
    pub fn read(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        path::read(path.as_ref())
    }

    /// Protocol version and entrance to use in the opening.
    pub const fn binding(&self) -> Binding {
        self.binding
    }

    /// Address of this participant's entrance.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Pinned board public key.
    pub const fn board_key(&self) -> &[u8; 32] {
        &self.board_key
    }

    /// The exact identity to claim.
    pub const fn id(&self) -> &ParticipantId {
        &self.id
    }

    /// Name of the mechanism the participant should use, or empty for the lead.
    pub fn mechanism(&self) -> &str {
        &self.mechanism
    }

    /// Opaque start data for the mechanism.
    pub fn mechanism_data(&self) -> &[u8] {
        &self.data
    }

    /// Borrow the private key for the Noise opening.
    pub fn private_key(&self) -> &[u8; 32] {
        &self.key
    }

    fn bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut encoded = Encoder::new(Vec::new());
        encoded
            .array(9)
            .unwrap()
            .u8(VERSION)
            .unwrap()
            .u16(self.binding.version)
            .unwrap()
            .u8(self.binding.entrance as u8)
            .unwrap()
            .str(&self.address)
            .unwrap()
            .bytes(&self.board_key)
            .unwrap()
            .str(self.id.as_str())
            .unwrap()
            .str(&self.mechanism)
            .unwrap()
            .bytes(&self.data)
            .unwrap()
            .bytes(self.key.as_slice())
            .unwrap();
        Zeroizing::new(encoded.into_writer())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        fn fields(bytes: &[u8]) -> Option<HandoverFile> {
            let mut d = Decoder::new(bytes);
            if d.array().ok()? != Some(9) || d.u8().ok()? != VERSION {
                return None;
            }
            let version = d.u16().ok()?;
            let entrance = match d.u8().ok()? {
                1 => Entrance::General,
                2 => Entrance::Lead,
                _ => return None,
            };
            let address = d.str().ok()?.to_owned();
            let board_key = d.bytes().ok()?.try_into().ok()?;
            let id = d.str().ok()?.parse().ok()?;
            let mechanism = d.str().ok()?.to_owned();
            let data = Zeroizing::new(d.bytes().ok()?.to_vec());
            let key = Zeroizing::new(d.bytes().ok()?.try_into().ok()?);
            if d.position() != bytes.len() {
                return None;
            }
            HandoverFile::new(
                Binding { version, entrance },
                address,
                board_key,
                id,
                mechanism,
                data,
                key,
            )
            .ok()
        }

        fields(bytes).ok_or_else(|| Error::refused("invalid handover file"))
    }
}

pub(crate) fn local_address(address: &str) -> bool {
    address
        .strip_prefix("unix:")
        .is_some_and(|path| std::path::Path::new(path).is_absolute() && !path.contains('\0'))
}

pub(crate) fn guest_address(address: &str) -> bool {
    address
        .strip_prefix("vsock:2:")
        .and_then(|port| port.parse::<u32>().ok())
        .is_some_and(|port| port != 0 && port != u32::MAX)
}
