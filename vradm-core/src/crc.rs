pub fn header_crc8(data: &[u8]) -> u8 {
    let mut crc = 0x00;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

pub fn payload_crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_crc8_vector() {
        // Test vector: ASCII "123456789" => 0xF4
        let data = b"123456789";
        assert_eq!(header_crc8(data), 0xF4);
    }

    #[test]
    fn test_payload_crc16_vector() {
        // Test vector: ASCII "123456789" => 0x29B1
        let data = b"123456789";
        assert_eq!(payload_crc16(data), 0x29B1);
    }
}
