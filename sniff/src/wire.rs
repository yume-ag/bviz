//! Protobuf wire format read from a prefix of the encoding, so a message the
//! prefix cuts off still yields its leading fields and every length it
//! declares.

use std::iter;

use prost::encoding::{WireType, decode_key, decode_varint};

/// A message as far as the prefix holds it.
#[derive(Clone, Copy)]
pub(crate) struct Message<'a> {
    /// The encoded length its delimiter or enclosing field declares.
    declared: usize,
    /// The first `declared` bytes of the encoding, or all the prefix holds.
    bytes: &'a [u8],
}

impl<'a> Message<'a> {
    fn within(prefix: &'a [u8], declared: usize) -> Self {
        Message {
            declared,
            bytes: &prefix[..declared.min(prefix.len())],
        }
    }

    pub(crate) fn is_whole(self) -> bool {
        self.bytes.len() == self.declared
    }

    pub(crate) fn fields(self) -> Fields<'a> {
        Fields {
            message: self,
            at: 0,
        }
    }

    /// Whether the message's fields end exactly where it does; true of a
    /// message the prefix cuts off, which cannot show otherwise.
    pub(crate) fn is_well_formed(self) -> bool {
        if !self.is_whole() {
            return true;
        }
        let mut fields = self.fields();
        while fields.next().is_some() {}
        fields.at_end()
    }
}

/// The length-delimited messages a stream's `prefix` begins, the last
/// possibly cut off.
pub(crate) fn delimited(prefix: &[u8]) -> impl Iterator<Item = Message<'_>> {
    let mut rest = prefix;
    iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let declared = prost::decode_length_delimiter(&mut rest).ok()?;
        let message = Message::within(rest, declared);
        rest = rest.get(declared..).unwrap_or_default();
        Some(message)
    })
}

pub(crate) struct Field<'a> {
    pub(crate) number: u32,
    pub(crate) value: Value<'a>,
}

pub(crate) enum Value<'a> {
    Varint(u64),
    Len(Message<'a>),
    Fixed,
}

/// A message's fields in order; ends at the message's end, at the end of the
/// prefix, or at an encoding the message cannot hold.
pub(crate) struct Fields<'a> {
    message: Message<'a>,
    at: usize,
}

const FIXED32_BYTES: usize = 4;
const FIXED64_BYTES: usize = 8;

impl<'a> Fields<'a> {
    /// Whether the fields read so far end exactly where the message does.
    pub(crate) fn at_end(&self) -> bool {
        self.at == self.message.declared
    }
}

/// A field's value read from `rest`, and the bytes it spans past `rest`'s
/// new start.
fn value<'a>(rest: &mut &'a [u8], wire: WireType) -> Option<(Value<'a>, usize)> {
    Some(match wire {
        WireType::Varint => (Value::Varint(decode_varint(rest).ok()?), 0),
        WireType::LengthDelimited => {
            let declared = usize::try_from(decode_varint(rest).ok()?).ok()?;
            (Value::Len(Message::within(rest, declared)), declared)
        }
        WireType::ThirtyTwoBit => (Value::Fixed, FIXED32_BYTES),
        WireType::SixtyFourBit => (Value::Fixed, FIXED64_BYTES),
        WireType::StartGroup | WireType::EndGroup => return None,
    })
}

impl<'a> Iterator for Fields<'a> {
    type Item = Field<'a>;

    fn next(&mut self) -> Option<Field<'a>> {
        if self.at >= self.message.declared {
            return None;
        }
        let mut rest = self.message.bytes.get(self.at..)?;
        let available = rest.len();
        let (number, wire) = decode_key(&mut rest).ok()?;
        let (value, payload) = value(&mut rest, wire)?;
        let end = self
            .at
            .checked_add(available - rest.len())?
            .checked_add(payload)?;
        if end > self.message.declared {
            return None;
        }
        self.at = end;
        Some(Field { number, value })
    }
}
