use sha2::{Digest, Sha256};
use snow::{
    Builder, HandshakeState, TransportState,
    params::NoiseParams,
    resolvers::{CryptoResolver, DefaultResolver},
};
use std::fmt;
use std::sync::LazyLock;
use zeroize::Zeroizing;

static SUITE: LazyLock<NoiseParams> = LazyLock::new(|| {
    "Noise_IK_25519_ChaChaPoly_BLAKE2s"
        .parse()
        .expect("known Noise suite")
});

/// Number of bytes in an IK opening with no payload.
pub const OPENING_BYTES: usize = 96;
/// Number of bytes in an IK reply with no payload.
pub const REPLY_BYTES: usize = 48;
/// Number of authentication tag bytes in an encrypted segment.
pub const TAG_BYTES: usize = 16;
/// Largest plaintext in one segment.
pub const MAX_CONTENT: usize = u16::MAX as usize - TAG_BYTES;
const PROTECTED_SEGMENTS: u64 = (1 << 32) - 1;

/// The authentication boundary a connection was opened for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Entrance {
    /// The general participant entrance.
    General = 1,
    /// The entrance reserved for the lead.
    Lead = 2,
    /// The owner door.
    Owner = 3,
}

/// Protocol context hashed into the Noise prologue.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    /// Protocol version named by the handover file.
    pub version: u16,
    /// Entrance the caller intends to use.
    pub entrance: Entrance,
}

impl Binding {
    fn prologue(self, board: &[u8; 32]) -> [u8; 40] {
        let mut prologue = [0; 40];
        prologue[..5].copy_from_slice(b"malak");
        prologue[5..7].copy_from_slice(&self.version.to_be_bytes());
        prologue[7] = self.entrance as u8;
        prologue[8..].copy_from_slice(&Sha256::digest(board));
        prologue
    }
}

/// An X25519 static keypair, for one handshake role only.
pub struct KeyPair {
    private: Zeroizing<[u8; 32]>,
    public: [u8; 32],
}

impl KeyPair {
    /// Draw a keypair from the operating system.
    pub fn generate() -> Result<Self, HandshakeError> {
        let key = Builder::new(SUITE.clone())
            .generate_keypair()
            .map_err(|_| HandshakeError::Setup)?;
        let private = Zeroizing::new(key.private);
        let public = key
            .public
            .as_slice()
            .try_into()
            .map_err(|_| HandshakeError::Setup)?;
        let mut secret = Zeroizing::new([0; 32]);
        if private.len() != secret.len() {
            return Err(HandshakeError::Setup);
        }
        secret.copy_from_slice(&private);
        Ok(Self {
            private: secret,
            public,
        })
    }

    /// Restore a static key pair from a saved private key.
    pub fn from_private(private: &[u8; 32]) -> Result<Self, HandshakeError> {
        Ok(Self {
            private: Zeroizing::new(*private),
            public: Self::public_for(private)?,
        })
    }

    /// Return the raw public half.
    pub const fn public(&self) -> &[u8; 32] {
        &self.public
    }

    /// Borrow the private half for a private handover or state file.
    pub fn private(&self) -> &[u8; 32] {
        &self.private
    }

    /// Derive the public key for a private key loaded from a handover.
    pub fn public_for(private: &[u8; 32]) -> Result<[u8; 32], HandshakeError> {
        let mut exchange = DefaultResolver
            .resolve_dh(&SUITE.dh)
            .ok_or(HandshakeError::Setup)?;
        exchange.set(private);
        exchange
            .pubkey()
            .try_into()
            .map_err(|_| HandshakeError::Setup)
    }
}

/// Why a local handshake or segment operation failed.
///
/// The driver closes on failure and sends no detail to the remote party.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandshakeError {
    /// The library could not initialise the requested cryptographic state.
    Setup,
    /// The supplied message length or output buffer is invalid.
    Length,
    /// The received message did not authenticate in this state.
    Authentication,
    /// The encrypted state has closed or reached its segment limit.
    Closed,
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Setup => "could not set up Noise",
            Self::Length => "invalid Noise message length",
            Self::Authentication => "Noise authentication failed",
            Self::Closed => "Noise transport state is closed",
        };
        output.write_str(reason)
    }
}

impl std::error::Error for HandshakeError {}

/// Participant's handshake state after producing its opening.
pub struct Initiator(HandshakeState);

impl Initiator {
    /// Produce the opening for a pinned board at one entrance and version.
    pub fn open(
        binding: Binding,
        caller_key: &[u8; 32],
        pinned_board: &[u8; 32],
    ) -> Result<(Self, [u8; OPENING_BYTES]), HandshakeError> {
        let prologue = binding.prologue(pinned_board);
        let mut state = Builder::new(SUITE.clone())
            .prologue(&prologue)
            .and_then(|builder| builder.local_private_key(caller_key))
            .and_then(|builder| builder.remote_public_key(pinned_board))
            .and_then(Builder::build_initiator)
            .map_err(|_| HandshakeError::Setup)?;
        let mut opening = [0; OPENING_BYTES];
        write_empty(&mut state, &mut opening)?;
        Ok((Self(state), opening))
    }

    /// Consume the board's reply and establish protected segment keys.
    pub fn finish(mut self, reply: &[u8]) -> Result<Encrypted, HandshakeError> {
        read_empty(&mut self.0, reply, REPLY_BYTES)?;
        Encrypted::from_handshake(self.0)
    }
}

/// Board's handshake state after reading an opening and before replying.
pub struct Responder {
    state: HandshakeState,
    caller_key: [u8; 32],
}

impl Responder {
    /// Read an opening for the specified entrance and protocol version.
    pub fn read(
        binding: Binding,
        board_private: &[u8; 32],
        board_public: &[u8; 32],
        opening: &[u8],
    ) -> Result<Self, HandshakeError> {
        let prologue = binding.prologue(board_public);
        let mut state = Builder::new(SUITE.clone())
            .prologue(&prologue)
            .and_then(|builder| builder.local_private_key(board_private))
            .and_then(Builder::build_responder)
            .map_err(|_| HandshakeError::Setup)?;
        read_empty(&mut state, opening, OPENING_BYTES)?;
        let caller_key = state
            .get_remote_static()
            .and_then(|key| key.try_into().ok())
            .ok_or(HandshakeError::Authentication)?;
        Ok(Self { state, caller_key })
    }

    /// The key in the opening, available for admission filtering before a reply.
    ///
    /// Possession remains unproven until the caller sends a valid protected frame.
    pub const fn caller_key(&self) -> &[u8; 32] {
        &self.caller_key
    }

    /// Produce the reply after applying the entrance's key filter.
    pub fn reply(mut self) -> Result<(Encrypted, [u8; REPLY_BYTES]), HandshakeError> {
        let mut reply = [0; REPLY_BYTES];
        write_empty(&mut self.state, &mut reply)?;
        Ok((Encrypted::from_handshake(self.state)?, reply))
    }
}

fn write_empty(state: &mut HandshakeState, output: &mut [u8]) -> Result<(), HandshakeError> {
    match state.write_message(&[], output) {
        Ok(length) if length == output.len() => Ok(()),
        _ => Err(HandshakeError::Setup),
    }
}

fn read_empty(
    state: &mut HandshakeState,
    input: &[u8],
    expected: usize,
) -> Result<(), HandshakeError> {
    if input.len() != expected {
        return Err(HandshakeError::Length);
    }
    match state.read_message(input, &mut []) {
        Ok(0) => Ok(()),
        _ => Err(HandshakeError::Authentication),
    }
}

/// Counter-ordered protection for one cryptographic connection.
///
/// The driver enforces elapsed lifetime and wraps each result in the segment
/// length prefix. It must deliver segments in the order in which they were sealed.
pub struct Encrypted {
    state: Option<TransportState>,
    handshake_hash: [u8; 32],
}

impl Encrypted {
    fn from_handshake(state: HandshakeState) -> Result<Self, HandshakeError> {
        let handshake_hash = state
            .get_handshake_hash()
            .try_into()
            .map_err(|_| HandshakeError::Setup)?;
        let state = state
            .into_transport_mode()
            .map_err(|_| HandshakeError::Setup)?;
        Ok(Self {
            state: Some(state),
            handshake_hash,
        })
    }

    /// The connection-specific final Noise hash available to the mechanism.
    pub const fn handshake_hash(&self) -> &[u8; 32] {
        &self.handshake_hash
    }

    /// Whether failure or the segment count has closed this cryptographic state.
    pub const fn is_closed(&self) -> bool {
        self.state.is_none()
    }

    /// Protect nonempty content, returning the length including the tag.
    ///
    /// The output buffer must hold the plaintext plus its tag.
    pub fn seal(&mut self, content: &[u8], output: &mut [u8]) -> Result<usize, HandshakeError> {
        self.process(|state| {
            if !(1..=MAX_CONTENT).contains(&content.len())
                || output.len() < content.len() + TAG_BYTES
            {
                return Err(HandshakeError::Length);
            }
            state
                .write_message(content, output)
                .map_err(|_| HandshakeError::Authentication)
        })
    }

    /// Authenticate one segment and return the number of plaintext bytes.
    ///
    /// An invalid, repeated or out-of-order segment closes this state permanently.
    pub fn open(&mut self, segment: &[u8], output: &mut [u8]) -> Result<usize, HandshakeError> {
        self.process(|state| {
            if !(TAG_BYTES + 1..=u16::MAX as usize).contains(&segment.len())
                || output.len() < segment.len() - TAG_BYTES
            {
                return Err(HandshakeError::Length);
            }
            state
                .read_message(segment, output)
                .map_err(|_| HandshakeError::Authentication)
        })
    }

    fn process(
        &mut self,
        operation: impl FnOnce(&mut TransportState) -> Result<usize, HandshakeError>,
    ) -> Result<usize, HandshakeError> {
        let state = self.state.as_mut().ok_or(HandshakeError::Closed)?;
        let result = operation(state);
        // The opening or reply accounts for the first segment in each direction.
        if result.is_err()
            || state.sending_nonce() >= PROTECTED_SEGMENTS
            || state.receiving_nonce() >= PROTECTED_SEGMENTS
        {
            self.state = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_closes_after_its_final_protected_segment() {
        let binding = Binding {
            version: 1,
            entrance: Entrance::General,
        };
        let caller = KeyPair::generate().unwrap();
        let board = KeyPair::generate().unwrap();
        let (mut initiator, opening) =
            Initiator::open(binding, caller.private(), board.public()).unwrap();
        let (mut encrypted, reply) =
            Responder::read(binding, board.private(), board.public(), &opening)
                .unwrap()
                .reply()
                .unwrap();
        read_empty(&mut initiator.0, &reply, REPLY_BYTES).unwrap();
        let sender = initiator.0.into_stateless_transport_mode().unwrap();
        let final_nonce = (1_u64 << 32) - 2;
        encrypted
            .state
            .as_mut()
            .unwrap()
            .set_receiving_nonce(final_nonce - 1);

        let mut ciphertext = [0; 1 + TAG_BYTES];
        let mut plaintext = [0; 1];
        for nonce in [final_nonce - 1, final_nonce] {
            sender.write_message(nonce, b"x", &mut ciphertext).unwrap();
            assert_eq!(encrypted.open(&ciphertext, &mut plaintext), Ok(1));
            assert_eq!(&plaintext, b"x");
        }
        assert!(encrypted.is_closed());
        assert_eq!(
            encrypted.open(&ciphertext, &mut plaintext),
            Err(HandshakeError::Closed)
        );
    }
}
