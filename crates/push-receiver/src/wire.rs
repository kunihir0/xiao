use crate::{Error, Result};

/// Checks length-delimited occurrences of one protobuf field without creating
/// the repeated field's decoded `Vec`. This runs before prost materialization
/// so a bounded wire payload cannot contain an unbounded number of empty
/// entries.
pub(crate) fn preflight_repeated_len_delimited(
    payload: &[u8],
    field_number: u64,
    max_count: usize,
    max_entry_len: usize,
) -> Result<()> {
    let mut offset = 0;
    let mut count = 0usize;
    while offset < payload.len() {
        let key = read_varint(payload, &mut offset)?;
        let number = key >> 3;
        let wire_type = key & 0x07;
        if number == 0 {
            return Err(Error::Decode("protobuf field number is zero".into()));
        }
        match wire_type {
            0 => {
                let _ = read_varint(payload, &mut offset)?;
            }
            1 => skip(payload, &mut offset, 8)?,
            2 => {
                let length = usize::try_from(read_varint(payload, &mut offset)?).map_err(|_| {
                    Error::FrameTooLarge {
                        actual: usize::MAX,
                        max: max_entry_len,
                    }
                })?;
                let end = offset.checked_add(length).ok_or(Error::FrameTooLarge {
                    actual: usize::MAX,
                    max: max_entry_len,
                })?;
                if end > payload.len() {
                    return Err(Error::Decode(
                        "protobuf length-delimited field is truncated".into(),
                    ));
                }
                if number == field_number {
                    count = count.checked_add(1).ok_or(Error::Decode(
                        "protobuf repeated field count overflow".into(),
                    ))?;
                    if count > max_count {
                        return Err(Error::Decode(
                            "protobuf repeated field exceeds item bound".into(),
                        ));
                    }
                    if length > max_entry_len {
                        return Err(Error::FrameTooLarge {
                            actual: length,
                            max: max_entry_len,
                        });
                    }
                }
                offset = end;
            }
            5 => skip(payload, &mut offset, 4)?,
            _ => return Err(Error::Decode("unsupported protobuf wire type".into())),
        }
    }
    Ok(())
}

fn read_varint(payload: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *payload
            .get(*offset)
            .ok_or_else(|| Error::Decode("truncated protobuf varint".into()))?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(Error::InvalidVarint);
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::InvalidVarint)
}

fn skip(payload: &[u8], offset: &mut usize, length: usize) -> Result<()> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| Error::Decode("protobuf field length overflow".into()))?;
    if end > payload.len() {
        return Err(Error::Decode("truncated protobuf field".into()));
    }
    *offset = end;
    Ok(())
}
