//! NetPacket protobuf wire 的受限解析工具.

const MAX_FIELD_NUMBER: u64 = (1 << 29) - 1;

#[derive(Debug, Clone, Copy)]
pub(crate) struct WireField<'a> {
    pub(crate) number: u32,
    pub(crate) wire_type: u8,
    pub(crate) start: usize,
    pub(crate) tag_end: usize,
    pub(crate) end: usize,
    pub(crate) value: WireValue<'a>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

pub(crate) fn parse_field<'a>(input: &'a [u8], cursor: &mut usize) -> Option<WireField<'a>> {
    let start = *cursor;
    let (tag, tag_end) = decode_varint(input, start)?;
    let number = tag >> 3;
    let wire_type = (tag & 0x07) as u8;
    if number == 0 || number > MAX_FIELD_NUMBER {
        return None;
    }

    let mut next = tag_end;
    let value = match wire_type {
        0 => {
            let (value, end) = decode_varint(input, next)?;
            next = end;
            WireValue::Varint(value)
        }
        1 => {
            next = next.checked_add(8)?;
            if next > input.len() {
                return None;
            }
            WireValue::Fixed
        }
        2 => {
            let (length, data_start) = decode_varint(input, next)?;
            let length = usize::try_from(length).ok()?;
            next = data_start.checked_add(length)?;
            let bytes = input.get(data_start..next)?;
            WireValue::Bytes(bytes)
        }
        5 => {
            next = next.checked_add(4)?;
            if next > input.len() {
                return None;
            }
            WireValue::Fixed
        }
        _ => return None,
    };

    *cursor = next;
    Some(WireField {
        number: number as u32,
        wire_type,
        start,
        tag_end,
        end: next,
        value,
    })
}

pub(crate) fn decode_varint(input: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = *input.get(start.checked_add(index)?)?;
        if index == 9 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, start + index + 1));
        }
    }
    None
}

pub(crate) fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}
