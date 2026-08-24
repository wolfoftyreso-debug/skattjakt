//! Standard base64, decode only.
//!
//! Hand-rolled for the same reason the API's copy is: this crate is compiled
//! into a WebAssembly module that reads other people's financial documents, and
//! every dependency it carries is one more thing in that module's supply chain.
//! Forty lines against a crate is a trade worth making here.

/// Decodes standard base64. `None` on any character outside the alphabet, so a
/// truncated or corrupted upload fails at the edge rather than as a strange
/// extraction three stages later.
pub fn base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, c) in TABLE.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }

    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();

    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in cleaned {
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn it_decodes_what_the_function_will_send() {
        assert_eq!(base64("aGVq"), Some(b"hej".to_vec()));
        assert_eq!(base64("aGVqIQ=="), Some(b"hej!".to_vec()));
        assert_eq!(base64(""), Some(Vec::new()));
        // Newlines are what a shell or a JSON pretty-printer inserts.
        assert_eq!(base64("aGVq\nIQ=="), Some(b"hej!".to_vec()));
    }

    #[test]
    fn a_corrupt_upload_is_refused_rather_than_half_decoded() {
        assert_eq!(base64("aGV*"), None);
        assert_eq!(base64("hej världen"), None);
    }

    /// Swedish text survives the round trip; the documents are full of å, ä, ö.
    #[test]
    fn utf8_survives() {
        let text = "Räkenskapsår 2025 — Nettoomsättning";
        let encoded = {
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let bytes = text.as_bytes();
            let mut s = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..4 {
                    if i <= chunk.len() {
                        s.push(T[((n >> (18 - i * 6)) & 63) as usize] as char);
                    } else {
                        s.push('=');
                    }
                }
            }
            s
        };
        assert_eq!(base64(&encoded).as_deref(), Some(text.as_bytes()));
    }
}
