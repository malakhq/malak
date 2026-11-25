use crate::Error;
use bytes::{Bytes, BytesMut};
use malak_layers::{Link, Received, SenderPeer, Source, Transport};
use malak_proto::{Encrypted, MAX_CONTENT, SegmentDecoder, SegmentKind, TAG_BYTES};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Ordered byte I/O for an accepted link. Framing and Noise remain in malak-proto.
pub(crate) struct Wire<T: Transport> {
    transport: Arc<T>,
    link: Link<T::Connection>,
    closed: AtomicBool,
}

impl<T: Transport> Wire<T> {
    pub(crate) fn new(transport: Arc<T>, link: Link<T::Connection>) -> Self {
        Self {
            transport,
            link,
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn sender(&self) -> &Arc<SenderPeer> {
        self.link.sender.observe()
    }

    pub(crate) fn process_id(&self) -> Option<u32> {
        self.link.process_id
    }

    pub(crate) fn source(&self) -> Option<&Source> {
        self.link.source.as_ref()
    }

    pub(crate) fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.transport.send(&self.link.io, &[]);
        }
    }

    fn receive(&self, output: &mut [u8]) -> Result<usize, Error> {
        match self.transport.receive(&self.link.io, output)? {
            Received::Bytes(count) if count.get() <= output.len() => Ok(count.get()),
            _ => Err(Error::unavailable("link closed while reading a segment")),
        }
    }

    fn write(&self, data: &[u8]) -> Result<(), Error> {
        self.write_with(data, |_| Ok(()))
    }

    pub(crate) fn write_with(
        &self,
        mut data: &[u8],
        mut progress: impl FnMut(usize) -> Result<(), Error>,
    ) -> Result<(), Error> {
        while !data.is_empty() {
            let count = self.transport.send(&self.link.io, data)?;
            if !(1..=data.len()).contains(&count) {
                return Err(Error::unavailable("link stopped while writing a segment"));
            }
            progress(count)?;
            data = &data[count..];
        }
        Ok(())
    }
}

impl<T: Transport> Drop for Wire<T> {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct Segments {
    decoder: SegmentDecoder,
    buffered: Bytes,
}

impl Segments {
    pub(crate) fn new() -> Self {
        Self {
            decoder: SegmentDecoder::default(),
            buffered: Bytes::new(),
        }
    }

    pub(crate) fn receive<T: Transport>(
        &mut self,
        wire: &Wire<T>,
        kind: SegmentKind,
    ) -> Result<Bytes, Error> {
        self.receive_with(wire, kind, |_| Ok(()))
    }

    pub(crate) fn receive_with<T: Transport>(
        &mut self,
        wire: &Wire<T>,
        kind: SegmentKind,
        mut progress: impl FnMut(Instant) -> Result<(), Error>,
    ) -> Result<Bytes, Error> {
        loop {
            if let Some(segment) = self
                .decoder
                .pull(kind, &mut self.buffered)
                .map_err(|_| Error::refused("invalid segment"))?
            {
                return Ok(segment);
            }
            let mut buffer = BytesMut::zeroed(4096);
            let count = wire.receive(&mut buffer)?;
            progress(Instant::now())?;
            buffer.truncate(count);
            self.buffered = buffer.freeze();
        }
    }

    pub(crate) fn send<T: Transport>(
        wire: &Wire<T>,
        kind: SegmentKind,
        segment: &[u8],
    ) -> Result<(), Error> {
        let prefix = SegmentDecoder::prefix(kind, segment)
            .map_err(|_| Error::refused("invalid segment length"))?;
        wire.write(&prefix)?;
        wire.write(segment)
    }

    pub(crate) fn open<T: Transport>(
        &mut self,
        wire: &Wire<T>,
        encrypted: &mut Encrypted,
    ) -> Result<Bytes, Error> {
        self.open_with(wire, encrypted, |_| Ok(()))
    }

    pub(crate) fn open_with<T: Transport>(
        &mut self,
        wire: &Wire<T>,
        encrypted: &mut Encrypted,
        progress: impl FnMut(Instant) -> Result<(), Error>,
    ) -> Result<Bytes, Error> {
        let segment = self.receive_with(wire, SegmentKind::Protected, progress)?;
        Self::decrypt(encrypted, &segment)
    }

    pub(crate) fn decrypt(encrypted: &mut Encrypted, segment: &[u8]) -> Result<Bytes, Error> {
        let expected = segment.len() - TAG_BYTES;
        let mut plaintext = BytesMut::zeroed(expected);
        encrypted
            .open(segment, &mut plaintext)
            .map_err(|_| Error::refused("invalid protected segment"))?;
        Ok(plaintext.freeze())
    }

    pub(crate) fn write_protected<T: Transport>(
        wire: &Wire<T>,
        encrypted: &mut Encrypted,
        plaintext: &[u8],
    ) -> Result<(), Error> {
        for chunk in plaintext.chunks(MAX_CONTENT) {
            let packet = Self::packet(encrypted, chunk)?;
            wire.write(&packet)?;
        }
        Ok(())
    }

    pub(crate) fn packet(encrypted: &mut Encrypted, chunk: &[u8]) -> Result<Vec<u8>, Error> {
        let mut packet = vec![0; 2 + chunk.len() + TAG_BYTES];
        let length = encrypted
            .seal(chunk, &mut packet[2..])
            .map_err(|_| Error::unavailable("could not protect segment"))?;
        let prefix = SegmentDecoder::prefix(SegmentKind::Protected, &packet[2..2 + length])
            .map_err(|_| Error::refused("invalid segment length"))?;
        packet[..2].copy_from_slice(&prefix);
        packet.truncate(2 + length);
        Ok(packet)
    }
}
