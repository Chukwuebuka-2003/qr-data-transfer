//! CRC-32 with the same polynomial and byte order as the JavaScript build.

const POLYNOMIAL: u32 = 0xedb8_8320;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut value = n as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                POLYNOMIAL ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[n] = value;
        n += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

/// Compute the CRC-32 of `bytes` (reflected, init `0xffffffff`, final xor).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in bytes {
        crc = TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn matches_known_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"QFC4"), 0xdc02_aeda);
    }
}
