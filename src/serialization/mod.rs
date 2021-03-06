mod bytes;
mod noop;
mod raw;
mod str;

pub use self::bytes::{BytesDeserializer, BytesSerializer};
pub use self::noop::{NoopDeserializer, NoopSerializer};
pub use self::raw::{RawDeserializer, RawSerializer};
pub use self::str::{StringDeserializer, StringSerializer};

#[cfg(feature = "encoding")]
mod encoding;
#[cfg(feature = "encoding")]
pub use self::encoding::{StrEncodingDeserializer, StrEncodingSerializer};

#[cfg(feature = "json")]
mod json;
#[cfg(feature = "json")]
pub use self::json::{JsonDeserializer, JsonSerializer};

use std::mem;
use std::result::Result;

use bytes::buf::FromBuf;
use bytes::{Buf, BufMut, Bytes};

/// A trait for serializing type to Kafka record
pub trait Serializer {
    /// The type of value that this serializer will serialize.
    type Item;
    /// The type of error that this serializer will return if it fails.
    type Error;

    /// Serizalize data of topic to the given buffer
    fn serialize_to<B: BufMut>(&self, topic_name: &str, data: Self::Item, buf: &mut B) -> Result<(), Self::Error>;

    /// Serialize data of topic as `Bytes`
    fn serialize(&self, topic_name: &str, data: Self::Item) -> Result<Bytes, Self::Error> {
        let mut buf = Vec::with_capacity(16);
        self.serialize_to(topic_name, data, &mut buf)?;
        Ok(Bytes::from_buf(buf))
    }
}

/// A trait for deserializing type from Kafka record
pub trait Deserializer {
    /// The type of value that this deserializer will deserialize.
    type Item;
    /// The type of error that this deserializer will return if it fails.
    type Error;

    /// Deserizalize data of topic from the given buffer
    fn deserialize_to<B: Buf>(&self, topic_name: &str, buf: &mut B, data: &mut Self::Item) -> Result<(), Self::Error>;

    fn deserialize<B: Buf>(&self, topic_name: &str, buf: &mut B) -> Result<Self::Item, Self::Error> {
        let mut data = unsafe { mem::zeroed() };

        self.deserialize_to(topic_name, buf, &mut data)?;

        Ok(data)
    }
}
