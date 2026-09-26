use crate::crc::{header_crc8, payload_crc16};
use crate::fec::{rs_encode_64_48, rs_encode_16_8, rs_decode_64_48, rs_decode_16_8};

// Modulo-256 Serial Number Arithmetic
#[inline]
pub fn seq_after(a: u8, b: u8) -> bool {
    (a != b) && (a.wrapping_sub(b) < 128)
}

#[inline]
pub fn seq_after_eq(a: u8, b: u8) -> bool {
    a.wrapping_sub(b) < 128
}

#[inline]
pub fn seq_diff(a: u8, b: u8) -> u8 {
    a.wrapping_sub(b)
}

#[inline]
pub fn seq_advance(s: u8, n: u8) -> u8 {
    s.wrapping_add(n)
}

// 8x8 Byte Block Interleaver
pub fn interleave_8x8(input: &[u8; 64], output: &mut [u8; 64]) {
    for i in 0..64 {
        let pi = (i % 8) * 8 + (i / 8);
        output[pi] = input[i];
    }
}

pub fn deinterleave_8x8(input: &[u8; 64], output: &mut [u8; 64]) {
    interleave_8x8(input, output);
}

pub const SYNC_WORD: u16 = 0xD391;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalDataFrame {
    pub ctrl: u8,
    pub seq: u8,
    pub ack_base: u8,
    pub ack_map: u8,
    pub payload_len: u8,
    pub payload: [u8; 38],
}

impl CanonicalDataFrame {
    pub fn new() -> Self {
        Self {
            ctrl: 0x02, // Current v3.8 IP wire version
            seq: 0,
            ack_base: 0,
            ack_map: 0,
            payload_len: 0,
            payload: [0; 38],
        }
    }

    pub fn encode(&self) -> [u8; 64] {
        let mut info = [0u8; 48];
        info[0..2].copy_from_slice(&SYNC_WORD.to_be_bytes());
        info[2] = self.ctrl;
        info[3] = self.seq;
        info[4] = self.ack_base;
        info[5] = self.ack_map;
        info[6] = self.payload_len.min(38); // max payload len 38
        info[7] = header_crc8(&info[2..7]);
        
        let len = info[6] as usize;
        info[8..8 + len].copy_from_slice(&self.payload[..len]);
        // Rest of payload is already 0-padded because info array is initialized with 0
        
        let crc16 = payload_crc16(&info[2..46]);
        info[46..48].copy_from_slice(&crc16.to_be_bytes());
        
        let codeword = rs_encode_64_48(&info);
        
        let mut interleaved = [0u8; 64];
        interleave_8x8(&codeword, &mut interleaved);
        interleaved
    }

    pub fn decode(interleaved: &[u8; 64], erasures: &[usize]) -> Result<Self, &'static str> {
        let mut codeword = [0u8; 64];
        deinterleave_8x8(interleaved, &mut codeword);
        
        let mut logical_erasures = [0usize; 16];
        let mut log_count = 0;
        for &e in erasures {
            if e < 64 && log_count < 16 {
                logical_erasures[log_count] = (e % 8) * 8 + (e / 8);
                log_count += 1;
            }
        }
        
        rs_decode_64_48(&mut codeword, &logical_erasures[..log_count]).map_err(|_| "RS decode failed")?;

        if u16::from_be_bytes([codeword[0], codeword[1]]) != SYNC_WORD {
            return Err("Invalid SYNC_WORD");
        }
        
        if header_crc8(&codeword[2..7]) != codeword[7] {
            return Err("Header CRC8 mismatch");
        }
        
        if payload_crc16(&codeword[2..46]) != u16::from_be_bytes([codeword[46], codeword[47]]) {
            return Err("Payload CRC16 mismatch");
        }
        
        let mut frame = Self::new();
        frame.ctrl = codeword[2];
        frame.seq = codeword[3];
        frame.ack_base = codeword[4];
        frame.ack_map = codeword[5];
        frame.payload_len = codeword[6];
        if frame.payload_len > 38 {
            return Err("Invalid payload length");
        }
        
        let len = frame.payload_len as usize;
        frame.payload[..len].copy_from_slice(&codeword[8..8 + len]);
        
        Ok(frame)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactControlFrame {
    pub ccf_ctrl: u8,
    pub ack_base: u8,
    pub ack_map: u8,
    pub ccf_mac: u8,
}

impl CompactControlFrame {
    pub fn new() -> Self {
        Self {
            ccf_ctrl: 0,
            ack_base: 0,
            ack_map: 0,
            ccf_mac: 0,
        }
    }

    pub fn encode(&self) -> [u8; 16] {
        let mut info = [0u8; 8];
        info[0..2].copy_from_slice(&SYNC_WORD.to_be_bytes());
        info[2] = self.ccf_ctrl;
        info[3] = self.ack_base;
        info[4] = self.ack_map;
        
        let crc16 = payload_crc16(&info[2..5]);
        info[5..7].copy_from_slice(&crc16.to_be_bytes());
        info[7] = self.ccf_mac;
        
        rs_encode_16_8(&info)
    }

    pub fn decode(mut codeword: [u8; 16], erasures: &[usize]) -> Result<Self, &'static str> {
        let mut valid_erasures = [0usize; 8];
        let mut valid_count = 0;
        for &e in erasures {
            if e < 16 && valid_count < 8 {
                valid_erasures[valid_count] = e;
                valid_count += 1;
            }
        }
        
        rs_decode_16_8(&mut codeword, &valid_erasures[..valid_count]).map_err(|_| "RS decode failed")?;

        if u16::from_be_bytes([codeword[0], codeword[1]]) != SYNC_WORD {
            return Err("Invalid SYNC_WORD");
        }
        
        if payload_crc16(&codeword[2..5]) != u16::from_be_bytes([codeword[5], codeword[6]]) {
            return Err("CCF CRC16 mismatch");
        }
        
        let mut frame = Self::new();
        frame.ccf_ctrl = codeword[2];
        frame.ack_base = codeword[3];
        frame.ack_map = codeword[4];
        frame.ccf_mac = codeword[7];
        
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seq_arithmetic() {
        assert!(seq_after(10, 5));
        assert!(seq_after(5, 250));
        assert!(!seq_after(5, 10));
        assert!(!seq_after(10, 10));

        assert!(seq_after_eq(10, 5));
        assert!(seq_after_eq(5, 250));
        assert!(!seq_after_eq(5, 10));
        assert!(seq_after_eq(10, 10));

        assert_eq!(seq_diff(10, 5), 5);
        assert_eq!(seq_diff(5, 250), 11);

        assert_eq!(seq_advance(250, 10), 4);
    }

    #[test]
    fn test_interleaver() {
        let mut input = [0u8; 64];
        for i in 0..64 {
            input[i] = i as u8;
        }

        let mut interleaved = [0u8; 64];
        interleave_8x8(&input, &mut interleaved);

        let mut deinterleaved = [0u8; 64];
        deinterleave_8x8(&interleaved, &mut deinterleaved);

        assert_eq!(input, deinterleaved);
        assert_eq!(interleaved[0], 0);
        assert_eq!(interleaved[1], 8);
        assert_eq!(interleaved[8], 1);
    }

    #[test]
    fn test_canonical_data_frame() {
        let mut frame = CanonicalDataFrame::new();
        frame.ctrl = 0b10101010;
        frame.seq = 42;
        frame.ack_base = 100;
        frame.ack_map = 0b01111110;
        frame.payload_len = 5;
        frame.payload[0..5].copy_from_slice(&[10, 20, 30, 40, 50]);

        let interleaved = frame.encode();
        
        let decoded = CanonicalDataFrame::decode(&interleaved, &[]).unwrap();
        assert_eq!(frame, decoded);

        // Test with some erasures and errors
        let mut corrupted = interleaved.clone();
        // Since it's interleaved, a burst error of 8 bytes corrupts one byte from each of 8 symbols.
        // Let's corrupt 3 bytes
        corrupted[10] ^= 0x55;
        corrupted[20] ^= 0xAA;
        corrupted[30] ^= 0xFF;

        let decoded_corrupted = CanonicalDataFrame::decode(&corrupted, &[]).unwrap();
        assert_eq!(frame, decoded_corrupted);
    }

    #[test]
    fn test_compact_control_frame() {
        let mut ccf = CompactControlFrame::new();
        ccf.ccf_ctrl = 0b11001100;
        ccf.ack_base = 200;
        ccf.ack_map = 0b10101010;
        ccf.ccf_mac = 0x99;

        let codeword = ccf.encode();
        let decoded = CompactControlFrame::decode(codeword, &[]).unwrap();

        assert_eq!(ccf, decoded);

        // Corrupt 2 bytes (RS(16,8) can correct 4 errors)
        let mut corrupted = codeword.clone();
        corrupted[1] ^= 0x12;
        corrupted[15] ^= 0x34;

        let decoded_corrupted = CompactControlFrame::decode(corrupted, &[]).unwrap();
        assert_eq!(ccf, decoded_corrupted);
    }

    #[test]
    fn test_canonical_data_frame_corrupted_sync() {
        let mut frame = CanonicalDataFrame::new();
        frame.ctrl = 0x02;
        frame.seq = 42;
        frame.ack_base = 40;
        frame.ack_map = 0x03;
        frame.payload_len = 10;
        frame.payload[..10].copy_from_slice(b"sync_test!");

        let interleaved = frame.encode();

        // Corrupt byte 0 of codeword before interleaving would correspond to interleaved[0]
        // because deinterleave_8x8 maps (0 % 8)*8 + (0 / 8) = 0.
        // Let's corrupt interleaved[0] which maps to codeword[0] (part of SYNC_WORD 0xD391).
        let mut corrupted = interleaved;
        corrupted[0] ^= 0xFF; // Corrupts high byte of SYNC_WORD from 0xD3 to 0x2C

        // RS decode should correct the corrupted sync byte and successfully decode
        let decoded = CanonicalDataFrame::decode(&corrupted, &[]).expect("RS decode should fix corrupted sync byte");
        assert_eq!(frame, decoded);
    }
}
