use crate::Error;
use malak_proto::Frame;
use minicbor::Decoder;
use std::collections::HashMap;

pub(crate) struct Fields<'a>(HashMap<&'a str, &'a [u8]>);

impl<'a> Fields<'a> {
    pub(crate) fn new(frame: &'a Frame) -> Result<Self, Error> {
        let bytes = frame.header();
        let mut input = Decoder::new(bytes);
        let count = input.map().map_err(|_| invalid())?.ok_or_else(invalid)?;
        let mut fields = HashMap::new();
        for _ in 0..count {
            let key = input.str().map_err(|_| invalid())?;
            let start = input.position();
            input.skip().map_err(|_| invalid())?;
            fields.insert(key, &bytes[start..input.position()]);
        }
        Ok(Self(fields))
    }

    pub(crate) fn get(&self, key: &str) -> Option<Decoder<'a>> {
        self.0.get(key).map(|bytes| Decoder::new(bytes))
    }

    pub(crate) fn only(&self, names: &[&str]) -> Result<(), Error> {
        if self
            .0
            .keys()
            .all(|key| matches!(*key, "op" | "rid") || names.contains(key))
        {
            Ok(())
        } else {
            Err(invalid())
        }
    }

    pub(crate) fn text(&self, key: &str) -> Result<Option<&'a str>, Error> {
        self.get(key)
            .map(|mut input| input.str())
            .transpose()
            .map_err(|_| invalid())
    }

    pub(crate) fn required(&self, key: &str) -> Result<&'a str, Error> {
        self.text(key)?.ok_or_else(invalid)
    }

    pub(crate) fn number(&self, key: &str) -> Result<Option<u64>, Error> {
        self.get(key)
            .map(|mut input| input.u64())
            .transpose()
            .map_err(|_| invalid())
    }
}

fn invalid() -> Error {
    Error::refused("invalid request fields")
}
