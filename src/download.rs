use std::io::{self, Read};

pub const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
pub const SIGNATURE_MAX_BYTES: u64 = 1024;

pub fn validate_size(size: u64, max_bytes: u64, label: &str) -> Result<(), String> {
    if size == 0 || size > max_bytes {
        return Err(format!(
            "{label} size must be between 1 and {max_bytes} bytes; got {size}."
        ));
    }
    Ok(())
}

pub fn read_response(
    response: reqwest::blocking::Response,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|size| size > max_bytes)
    {
        return Err(format!("{label} exceeds the {max_bytes}-byte size limit."));
    }
    read_bounded(response, max_bytes).map_err(|error| format!("Failed to read {label}: {error}"))
}

pub fn read_exact_response(
    response: reqwest::blocking::Response,
    expected_size: u64,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    validate_size(expected_size, max_bytes, label)?;
    if response
        .content_length()
        .is_some_and(|size| size != expected_size)
    {
        return Err(format!(
            "{label} size mismatch: expected {expected_size} bytes."
        ));
    }
    read_exact_bounded(response, expected_size)
        .map_err(|error| format!("Failed to read {label}: {error}"))
}

fn read_bounded(reader: impl Read, max_bytes: u64) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut body)?;
    if body.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("response exceeds the {max_bytes}-byte size limit"),
        ));
    }
    Ok(body)
}

fn read_exact_bounded(reader: impl Read, expected_size: u64) -> io::Result<Vec<u8>> {
    let body = read_bounded(reader, expected_size)?;
    if body.len() as u64 != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "size mismatch: expected {expected_size}, got {}",
                body.len()
            ),
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::{read_bounded, read_exact_bounded, validate_size};
    use std::io::{self, Cursor};

    #[test]
    fn accepts_a_body_exactly_at_the_limit() {
        assert_eq!(read_bounded(Cursor::new(b"abcd"), 4).unwrap(), b"abcd");
    }

    #[test]
    fn stops_an_unbounded_body_after_one_excess_byte() {
        let mut source = io::repeat(b'x');
        assert_eq!(
            read_bounded(&mut source, 16).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // A reader with no end must still terminate at the configured limit.
    }

    #[test]
    fn does_not_consume_the_rest_of_an_oversized_body() {
        let mut source = Cursor::new(b"0123456789");
        assert!(read_bounded(&mut source, 4).is_err());
        assert_eq!(source.position(), 5);
    }

    #[test]
    fn rejects_empty_or_oversized_signed_downloads() {
        assert!(validate_size(0, 16, "package").is_err());
        assert!(validate_size(17, 16, "package").is_err());
        assert!(validate_size(16, 16, "package").is_ok());
    }

    #[test]
    fn rejects_a_truncated_download_without_relying_on_headers() {
        assert_eq!(
            read_exact_bounded(Cursor::new(b"abc"), 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            read_exact_bounded(Cursor::new(b"abcd"), 4).unwrap(),
            b"abcd"
        );
    }
}
