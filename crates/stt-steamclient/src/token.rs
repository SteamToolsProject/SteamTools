//! PICS access token 的受限 frame 与 protobuf wire 改写.

use std::collections::HashMap;

use stt_core::AppId;

const BINARY_OPCODE: u32 = 2;
const PROTO_FLAG: u32 = 0x8000_0000;
const PICS_PRODUCT_INFO_REQUEST: u32 = 8903;
const FRAME_HEADER_SIZE: usize = 8;
const MAX_PROTO_HEADER_SIZE: usize = 1024;
const MAX_BODY_SIZE: usize = 65_536;
const MAX_FIELD_NUMBER: u64 = (1 << 29) - 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessTokenRewrite {
    Passthrough,
    Rewritten {
        packet: Vec<u8>,
        patched_apps: usize,
    },
}

#[derive(Debug, Clone, Copy)]
struct WireField<'a> {
    number: u32,
    wire_type: u8,
    start: usize,
    tag_end: usize,
    end: usize,
    value: WireValue<'a>,
}

#[derive(Debug, Clone, Copy)]
enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// 只改写二进制 EMsg 8903 frame 中已配置 app 的 access token.
pub fn rewrite_access_token_frame(
    opcode: u32,
    packet: &[u8],
    tokens: &HashMap<AppId, u64>,
) -> AccessTokenRewrite {
    if opcode != BINARY_OPCODE || tokens.is_empty() || packet.len() < FRAME_HEADER_SIZE {
        return AccessTokenRewrite::Passthrough;
    }

    let Some(message) = read_u32_le(packet, 0) else {
        return AccessTokenRewrite::Passthrough;
    };
    if message & PROTO_FLAG == 0 || message & !PROTO_FLAG != PICS_PRODUCT_INFO_REQUEST {
        return AccessTokenRewrite::Passthrough;
    }
    let Some(header_size) = read_u32_le(packet, 4).and_then(|value| usize::try_from(value).ok())
    else {
        return AccessTokenRewrite::Passthrough;
    };
    if header_size > MAX_PROTO_HEADER_SIZE {
        return AccessTokenRewrite::Passthrough;
    }
    let Some(body_offset) = FRAME_HEADER_SIZE.checked_add(header_size) else {
        return AccessTokenRewrite::Passthrough;
    };
    if body_offset > packet.len() || packet.len() - body_offset > MAX_BODY_SIZE {
        return AccessTokenRewrite::Passthrough;
    }

    let Some((body, patched_apps)) = rewrite_request_body(&packet[body_offset..], tokens) else {
        return AccessTokenRewrite::Passthrough;
    };
    if patched_apps == 0 || body.len() > MAX_BODY_SIZE {
        return AccessTokenRewrite::Passthrough;
    }

    let Some(new_size) = body_offset.checked_add(body.len()) else {
        return AccessTokenRewrite::Passthrough;
    };
    let mut rewritten = Vec::with_capacity(new_size);
    rewritten.extend_from_slice(&packet[..body_offset]);
    rewritten.extend_from_slice(&body);
    AccessTokenRewrite::Rewritten {
        packet: rewritten,
        patched_apps,
    }
}

fn rewrite_request_body(body: &[u8], tokens: &HashMap<AppId, u64>) -> Option<(Vec<u8>, usize)> {
    let mut cursor = 0;
    let mut output = Vec::with_capacity(body.len());
    let mut patched_apps = 0;
    while cursor < body.len() {
        let field = parse_field(body, &mut cursor)?;
        let WireValue::Bytes(app) = field.value else {
            output.extend_from_slice(&body[field.start..field.end]);
            continue;
        };
        if field.number != 2 || field.wire_type != 2 {
            output.extend_from_slice(&body[field.start..field.end]);
            continue;
        }

        let Some(rewritten_app) = rewrite_app_info(app, tokens)? else {
            output.extend_from_slice(&body[field.start..field.end]);
            continue;
        };
        output.extend_from_slice(&body[field.start..field.tag_end]);
        encode_varint(rewritten_app.len() as u64, &mut output);
        output.extend_from_slice(&rewritten_app);
        patched_apps += 1;
    }
    Some((output, patched_apps))
}

fn rewrite_app_info(app: &[u8], tokens: &HashMap<AppId, u64>) -> Option<Option<Vec<u8>>> {
    let mut cursor = 0;
    let mut app_id = None;
    let mut current_token = None;
    while cursor < app.len() {
        let field = parse_field(app, &mut cursor)?;
        match (field.number, field.wire_type, field.value) {
            (1, 0, WireValue::Varint(value)) => app_id = u32::try_from(value).ok(),
            (2, 0, WireValue::Varint(value)) => current_token = Some(value),
            _ => {}
        }
    }

    let token = app_id
        .and_then(|app_id| tokens.get(&app_id))
        .copied()
        .filter(|token| *token != 0);
    let Some(token) = token else {
        return Some(None);
    };
    if current_token == Some(token) {
        return Some(None);
    }

    let mut cursor = 0;
    let mut output = Vec::with_capacity(app.len() + 11);
    while cursor < app.len() {
        let field = parse_field(app, &mut cursor)?;
        if field.number != 2 || field.wire_type != 0 {
            output.extend_from_slice(&app[field.start..field.end]);
        }
    }
    encode_varint((2 << 3) as u64, &mut output);
    encode_varint(token, &mut output);
    Some(Some(output))
}

fn parse_field<'a>(input: &'a [u8], cursor: &mut usize) -> Option<WireField<'a>> {
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

fn decode_varint(input: &[u8], start: usize) -> Option<(u64, usize)> {
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

fn read_u32_le(input: &[u8], offset: usize) -> Option<u32> {
    let bytes = input.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_varint(number: u32, value: u64) -> Vec<u8> {
        let mut output = Vec::new();
        encode_varint(u64::from(number) << 3, &mut output);
        encode_varint(value, &mut output);
        output
    }

    fn field_bytes(number: u32, value: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        encode_varint((u64::from(number) << 3) | 2, &mut output);
        encode_varint(value.len() as u64, &mut output);
        output.extend_from_slice(value);
        output
    }

    fn frame(body: &[u8]) -> Vec<u8> {
        let header = [0xAA, 0xBB, 0xCC];
        let mut packet = Vec::new();
        packet.extend_from_slice(&(PROTO_FLAG | PICS_PRODUCT_INFO_REQUEST).to_le_bytes());
        packet.extend_from_slice(&(header.len() as u32).to_le_bytes());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(body);
        packet
    }

    #[test]
    fn configured_app_token_is_replaced_without_touching_other_fields() {
        let mut app = field_varint(1, 42);
        app.extend(field_bytes(9, b"unknown"));
        app.extend(field_varint(2, 7));
        let package = field_varint(1, 100);
        let mut body = field_bytes(1, &package);
        body.extend(field_varint(8, 99));
        body.extend(field_bytes(2, &app));
        body.extend(field_varint(3, 1));

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        let mut expected_app = field_varint(1, 42);
        expected_app.extend(field_bytes(9, b"unknown"));
        expected_app.extend(field_varint(2, 123));
        let mut expected_body = field_bytes(1, &package);
        expected_body.extend(field_varint(8, 99));
        expected_body.extend(field_bytes(2, &expected_app));
        expected_body.extend(field_varint(3, 1));
        assert_eq!(
            result,
            AccessTokenRewrite::Rewritten {
                packet: frame(&expected_body),
                patched_apps: 1,
            }
        );
    }

    #[test]
    fn unrelated_app_is_passthrough() {
        let app = field_varint(1, 42);
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(43, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn existing_matching_token_is_passthrough() {
        let mut app = field_varint(1, 42);
        app.extend(field_varint(2, 123));
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn non_binary_frame_is_passthrough() {
        let app = field_varint(1, 42);
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(1, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn malformed_nested_length_is_passthrough() {
        let body = [0x12, 0x04, 0x08, 0x2A];

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn oversized_body_is_passthrough() {
        let body = vec![0; MAX_BODY_SIZE + 1];

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn token_growth_past_body_limit_is_passthrough() {
        let mut app = field_varint(1, 42);
        let filler_size = MAX_BODY_SIZE - app.len() - 8;
        app.extend(field_bytes(9, &vec![0; filler_size]));
        let body = field_bytes(2, &app);
        assert_eq!(body.len(), MAX_BODY_SIZE);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, u64::MAX)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn overflowing_varint_is_passthrough() {
        let app = [
            0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02,
        ];
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 123)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }

    #[test]
    fn zero_snapshot_token_is_passthrough() {
        let app = field_varint(1, 42);
        let body = field_bytes(2, &app);

        let result = rewrite_access_token_frame(2, &frame(&body), &HashMap::from([(42, 0)]));

        assert_eq!(result, AccessTokenRewrite::Passthrough);
    }
}
