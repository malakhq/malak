use crate::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

/// The creation of a channel. A new creation of the same name gets a new ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CreationId([u8; 16]);

impl CreationId {
    /// Construct an ID from its sixteen bytes. The board draws new IDs at creation.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Return the binary form of this ID.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for CreationId {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(output, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for CreationId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        fn hex(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            }
        }

        let error = || Error::refused("invalid creation ID");
        let input: &[u8; 32] = value.as_bytes().try_into().map_err(|_| error())?;
        let mut bytes = [0; 16];
        for (byte, pair) in bytes.iter_mut().zip(input.as_chunks::<2>().0) {
            *byte = hex(pair[0]).ok_or_else(error)? * 16 + hex(pair[1]).ok_or_else(error)?;
        }
        Ok(Self(bytes))
    }
}

/// A one-based message position within one channel creation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Position(NonZeroU64);

impl Position {
    /// The first assignable position in any creation.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// The largest assignable position in any creation.
    pub const MAX: u64 = i64::MAX as u64;

    /// Return the one-based numeric position.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl TryFrom<u64> for Position {
    type Error = Error;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        NonZeroU64::new(value)
            .filter(|position| position.get() <= Self::MAX)
            .map(Self)
            .ok_or_else(|| Error::refused("invalid position"))
    }
}

impl fmt::Display for Position {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(output)
    }
}

/// The durability reached by a store on successful writes.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum Durability {
    /// Survives only for the life of the board.
    #[default]
    Memory,
    /// Survives killing the board process. A power cut can lose recent writes.
    Restart,
    /// Confirmed on stable storage before the board replies.
    Synced,
}

/// An action a participant grant can authorize on a channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Send a message.
    Send,
    /// Read due messages.
    Read,
    /// Get one message by its position.
    Get,
    /// Acknowledge delivery through a position.
    Ack,
    /// Create a channel.
    Create,
}

/// The proof held by a participant connection or handle.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Grade {
    /// The board can report what the transport knows about the link.
    LinkOnly,
    /// The board validated a registered key or issued the handle.
    Proven,
}
