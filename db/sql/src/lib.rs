//! Decodes the Hew `Vec<hew.db.sql.Param>` wire representation once for all
//! SQL clients. Driver modules only map these typed values to their own API.

use ciborium::Value;
use std::io::Cursor;

#[derive(Debug, PartialEq)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

/// Decode a complete parameter vector, rejecting unknown variants, invalid
/// payloads and trailing data before any database operation.
///
/// # Errors
/// Returns a diagnostic if the input does not match the typed parameter schema.
pub fn decode(data: &[u8]) -> Result<Vec<Param>, String> {
    let mut input = Cursor::new(data);
    let value: Value = ciborium::from_reader(&mut input)
        .map_err(|error| format!("invalid SQL parameter encoding: {error}"))?;
    if input.position() != data.len() as u64 {
        return Err("trailing data after SQL parameters".to_owned());
    }
    let Value::Array(values) = value else {
        return Err("SQL parameters must be an array".to_owned());
    };
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            decode_param(value).map_err(|error| format!("SQL parameter {}: {error}", index + 1))
        })
        .collect()
}

fn decode_param(value: Value) -> Result<Param, &'static str> {
    if let Value::Integer(tag) = &value {
        return if i128::from(*tag) == 0 {
            Ok(Param::Null)
        } else {
            Err("unknown unit variant")
        };
    }
    let Value::Map(mut entry) = value else {
        return Err("expected a typed parameter");
    };
    if entry.len() != 1 {
        return Err("expected one variant");
    }
    let (Value::Integer(tag), payload) = entry.pop().unwrap() else {
        return Err("invalid variant representation");
    };
    match (i128::from(tag), payload) {
        (1, Value::Bool(value)) => Ok(Param::Bool(value)),
        (2, Value::Integer(value)) => i64::try_from(value)
            .map(Param::Int)
            .map_err(|_| "integer exceeds i64"),
        (3, Value::Float(value)) => Ok(Param::Float(value)),
        (4, Value::Text(value)) => Ok(Param::Text(value)),
        (5, Value::Bytes(value)) => Ok(Param::Bytes(value)),
        _ => Err("unknown variant or invalid payload type"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_boundary_data() {
        for data in [
            &b""[..],
            &[0x80, 0],
            &[0x81, 6],
            &[0x81, 0xa1, 2, 0xf5],
            &[0x81, 0xa1, 2, 0x81, 7],
            &[0xa0],
        ] {
            assert!(decode(data).is_err(), "accepted {data:?}");
        }
        assert_eq!(decode(&[0x80]).unwrap(), Vec::new());
        assert_eq!(decode(&[0x81, 0]).unwrap(), vec![Param::Null]);
    }

    #[test]
    fn decodes_the_format_codec_representation() {
        // cbor.encode of [.Null, .Int(7), .Text("a")]: unit variants are their
        // tag, payload variants are a one-entry map from tag to payload.
        let data = [0x83, 0x00, 0xa1, 0x02, 0x07, 0xa1, 0x04, 0x61, 0x61];
        assert_eq!(
            decode(&data).unwrap(),
            vec![Param::Null, Param::Int(7), Param::Text("a".to_owned())]
        );
    }
}
