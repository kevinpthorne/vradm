const P12: [u16; 12] = [
    0xDC5, 0x6E3, 0xB71, 0x5B9, 0x2DD, 0x16F, 0x8B7, 0xC5B, 0xE2D, 0x717, 0xB8B, 0xFFE
];

pub fn golay_encode(m: u16) -> u32 {
    let mut p = 0u16;
    for i in 0..12 {
        if (m & (1 << (11 - i))) != 0 {
            p ^= P12[i];
        }
    }
    ((m as u32) << 12) | (p as u32)
}

pub fn golay_decode(c: u32) -> Result<u16, ()> {
    let m_recv = (c >> 12) as u16 & 0xFFF;
    let p_recv = (c & 0xFFF) as u16;
    let p_m = (golay_encode(m_recv) & 0xFFF) as u16;
    let s = p_m ^ p_recv;

    if s.count_ones() <= 3 {
        return Ok(m_recv);
    }

    for i in 0..12 {
        if (s ^ P12[i]).count_ones() <= 2 {
            return Ok(m_recv ^ (1 << (11 - i)));
        }
    }

    let mut s2 = 0u16;
    for j in 0..12 {
        if (s & P12[j]).count_ones() % 2 != 0 {
            s2 |= 1 << (11 - j);
        }
    }

    if s2.count_ones() <= 3 {
        return Ok(m_recv ^ s2);
    }

    let mut p12_t = [0u16; 12];
    for i in 0..12 {
        for j in 0..12 {
            if (P12[j] & (1 << (11 - i))) != 0 {
                p12_t[i] |= 1 << (11 - j);
            }
        }
    }

    for i in 0..12 {
        if (s2 ^ p12_t[i]).count_ones() <= 2 {
            return Ok(m_recv ^ s2 ^ p12_t[i]);
        }
    }

    Err(())
}

const fn generate_gf_tables() -> ([u8; 512], [usize; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0usize; 256];
    let mut x = 1u16;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        log[x as usize] = i;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11D;
        }
        i += 1;
    }
    (exp, log)
}

const GF_TABLES: ([u8; 512], [usize; 256]) = generate_gf_tables();
const EXP: [u8; 512] = GF_TABLES.0;
const LOG: [usize; 256] = GF_TABLES.1;

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 { return 0; }
    EXP[LOG[a as usize] + LOG[b as usize]]
}

fn div(a: u8, b: u8) -> u8 {
    if a == 0 { return 0; }
    EXP[LOG[a as usize] + 255 - LOG[b as usize]]
}

const GEN_64_48: [u8; 17] = [
    0x01, 0x3B, 0x0D, 0x68, 0xBD, 0x44, 0xD1, 0x1E, 0x08, 0xA3, 0x41, 0x29, 0xE5, 0x62, 0x32, 0x24, 0x3B
];

const GEN_16_8: [u8; 9] = [
    0x01, 0xFF, 0x0B, 0x51, 0x36, 0xEF, 0xAD, 0xC8, 0x18
];

fn rs_encode_generic(info: &[u8], gen: &[u8], parity: &mut [u8]) {
    for p in parity.iter_mut() { *p = 0; }
    let t2 = parity.len();
    for &b in info {
        let factor = parity[0] ^ b;
        for i in 0..t2 - 1 {
            parity[i] = parity[i+1] ^ mul(factor, gen[i+1]);
        }
        parity[t2 - 1] = mul(factor, gen[t2]);
    }
}

pub fn rs_encode_64_48(info: &[u8; 48]) -> [u8; 64] {
    let mut codeword = [0u8; 64];
    codeword[..48].copy_from_slice(info);
    let mut parity = [0u8; 16];
    rs_encode_generic(info, &GEN_64_48, &mut parity);
    codeword[48..].copy_from_slice(&parity);
    codeword
}

pub fn rs_encode_16_8(info: &[u8; 8]) -> [u8; 16] {
    let mut codeword = [0u8; 16];
    codeword[..8].copy_from_slice(info);
    let mut parity = [0u8; 8];
    rs_encode_generic(info, &GEN_16_8, &mut parity);
    codeword[8..].copy_from_slice(&parity);
    codeword
}

const MAX_T2: usize = 16;
const MAX_N: usize = 64;

fn rs_decode_generic(recv: &mut [u8], erasures: &[usize], n: usize, k: usize) -> Result<(), ()> {
    let t2 = n - k;
    if erasures.len() > t2 || t2 > MAX_T2 || n > MAX_N {
        return Err(());
    }

    // 1. Syndrome computation (stack array [u8; 16])
    let mut syn = [0u8; MAX_T2];
    let mut has_error = false;
    for j in 0..t2 {
        let root = EXP[j];
        let mut sum = 0u8;
        for i in 0..n {
            sum = mul(sum, root) ^ recv[i];
        }
        syn[j] = sum;
        if sum != 0 {
            has_error = true;
        }
    }
    if !has_error {
        return Ok(());
    }

    // 2. Erasure locator polynomial lambda(x) (stack array [u8; 18])
    let mut lambda = [0u8; MAX_T2 + 2];
    lambda[0] = 1;
    let mut lambda_deg = 0;
    for &pos in erasures {
        let x_j = EXP[(n - 1 - pos) % 255];
        let mut next = [0u8; MAX_T2 + 2];
        for i in 0..=lambda_deg {
            next[i] ^= lambda[i];
            next[i + 1] ^= mul(lambda[i], x_j);
        }
        lambda = next;
        lambda_deg += 1;
    }

    // 3. Modified syndromes T(x) (stack array [u8; 16])
    let mut t_syn = [0u8; MAX_T2];
    for i in 0..t2 {
        let mut sum = 0u8;
        for j in 0..=lambda_deg {
            if i >= j {
                sum ^= mul(lambda[j], syn[i - j]);
            }
        }
        t_syn[i] = sum;
    }

    // 4. Berlekamp-Massey iteration for error locator sigma(x)
    let mut sigma = [0u8; MAX_T2 + 2];
    sigma[0] = 1;
    let mut b = [0u8; MAX_T2 + 2];
    b[0] = 1;
    let mut l = 0;
    let mut m = 1;

    for i in lambda_deg..t2 {
        let mut d = 0u8;
        for j in 0..=l {
            d ^= mul(sigma[j], t_syn[i - j]);
        }
        if d == 0 {
            m += 1;
        } else {
            let mut next_sigma = sigma;
            for j in 0..=t2 {
                if j >= m {
                    next_sigma[j] ^= mul(d, b[j - m]);
                }
            }
            if 2 * l <= i - lambda_deg {
                b = sigma;
                for j in 0..=t2 {
                    b[j] = div(b[j], d);
                }
                l = i - lambda_deg + 1 - l;
                m = 1;
            } else {
                m += 1;
            }
            sigma = next_sigma;
        }
    }

    if 2 * l + lambda_deg > t2 {
        return Err(());
    }

    // 5. Total locator polynomial phi(x) = lambda(x) * sigma(x) (stack array [u8; 34])
    let mut phi = [0u8; MAX_T2 * 2 + 2];
    for i in 0..=lambda_deg {
        for j in 0..=l {
            if i + j <= t2 {
                phi[i + j] ^= mul(lambda[i], sigma[j]);
            }
        }
    }
    let deg_phi = lambda_deg + l;

    // 6. Chien search for error roots (stack array [usize; 16])
    let mut locs = [0usize; MAX_T2];
    let mut locs_len = 0;
    for i in 0..n {
        let x_inv = EXP[255 - (n - 1 - i) % 255];
        let mut sum = 0u8;
        let mut x_pow = 1u8;
        for j in 0..=deg_phi {
            sum ^= mul(phi[j], x_pow);
            x_pow = mul(x_pow, x_inv);
        }
        if sum == 0 {
            if locs_len >= MAX_T2 {
                return Err(());
            }
            locs[locs_len] = i;
            locs_len += 1;
        }
    }

    if locs_len != deg_phi {
        return Err(());
    }

    // 7. Error evaluator omega(x) = (phi(x) * syn(x)) mod x^t2 (stack array [u8; 16])
    let mut omega = [0u8; MAX_T2];
    for i in 0..t2 {
        let mut sum = 0u8;
        for j in 0..=deg_phi {
            if i >= j {
                sum ^= mul(phi[j], syn[i - j]);
            }
        }
        omega[i] = sum;
    }

    // 8. Forney algorithm for error magnitudes
    for &pos in &locs[..locs_len] {
        let x_inv = EXP[255 - (n - 1 - pos) % 255];
        let mut phi_prime = 0u8;
        let mut x_pow = 1u8;
        for j in (1..=deg_phi).step_by(2) {
            phi_prime ^= mul(phi[j], x_pow);
            x_pow = mul(x_pow, mul(x_inv, x_inv));
        }
        if phi_prime == 0 {
            return Err(());
        }

        let mut o_val = 0u8;
        let mut x_pow_o = 1u8;
        for j in 0..deg_phi {
            o_val ^= mul(omega[j], x_pow_o);
            x_pow_o = mul(x_pow_o, x_inv);
        }

        let x_j = EXP[(n - 1 - pos) % 255];
        let mag = mul(x_j, div(o_val, phi_prime));
        recv[pos] ^= mag;
    }

    // 9. Final syndrome integrity check
    for j in 0..t2 {
        let root = EXP[j];
        let mut sum = 0u8;
        for i in 0..n {
            sum = mul(sum, root) ^ recv[i];
        }
        if sum != 0 {
            return Err(());
        }
    }

    Ok(())
}

pub fn rs_decode_64_48(codeword: &mut [u8; 64], erasures: &[usize]) -> Result<(), ()> {
    rs_decode_generic(codeword, erasures, 64, 48)
}

pub fn rs_decode_16_8(codeword: &mut [u8; 16], erasures: &[usize]) -> Result<(), ()> {
    rs_decode_generic(codeword, erasures, 16, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_golay_encode_vector() {
        let m: u16 = 0x5A5;
        let expected_c: u32 = 0x5A51D9;
        let encoded = golay_encode(m);
        assert_eq!(encoded, expected_c, "Golay encoding test vector failed");
    }

    #[test]
    fn test_golay_decode_vector() {
        let expected_m: u16 = 0x5A5;
        let corrupted_c: u32 = 0xFA5199;
        let decoded = golay_decode(corrupted_c).expect("Golay decode failed");
        assert_eq!(decoded, expected_m, "Golay 3-bit error correction test vector failed");
    }

    #[test]
    fn test_reed_solomon_64_48_vectors() {
        let info_bytes: [u8; 48] = [
            0x56, 0x52, 0x41, 0x44, 0x4D, 0x5F, 0x54, 0x45, 0x53, 0x54, 0x5F, 0x46, 0x52, 0x41, 0x4D, 0x45,
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
        ];
        
        let expected_parity: [u8; 16] = [
            0x97, 0x65, 0x00, 0xFE, 0x8D, 0x05, 0x17, 0x4F,
            0x63, 0x7C, 0xFE, 0x74, 0x32, 0xAF, 0x3D, 0xEE,
        ];
        
        let codeword = rs_encode_64_48(&info_bytes);
        assert_eq!(&codeword[48..], &expected_parity, "RS(64, 48) parity mismatch");
        
        let mut corrupted = codeword.clone();
        corrupted[0] ^= 0x12;
        corrupted[10] ^= 0x34;
        rs_decode_64_48(&mut corrupted, &[]).expect("RS decode failed");
        assert_eq!(corrupted, codeword);
    }
    
    #[test]
    fn test_reed_solomon_16_8_vectors() {
        let info_bytes: [u8; 8] = [
            0xD3, 0x91, 0x01, 0x0A, 0x00, 0x29, 0xB1, 0x4F,
        ];
        
        let expected_parity: [u8; 8] = [
            0xEF, 0x1D, 0x1F, 0x6C, 0x64, 0xB6, 0x63, 0xAE,
        ];
        
        let codeword = rs_encode_16_8(&info_bytes);
        assert_eq!(&codeword[8..], &expected_parity, "RS(16, 8) parity mismatch");
        
        let mut corrupted = codeword.clone();
        corrupted[0] ^= 0x12;
        rs_decode_16_8(&mut corrupted, &[]).expect("RS decode failed");
        assert_eq!(corrupted, codeword);
    }

    #[test]
    fn test_reed_solomon_64_48_edge_cases() {
        let info_bytes: [u8; 48] = [
            0x56, 0x52, 0x41, 0x44, 0x4D, 0x5F, 0x54, 0x45, 0x53, 0x54, 0x5F, 0x46, 0x52, 0x41, 0x4D, 0x45,
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
        ];
        let codeword = rs_encode_64_48(&info_bytes);
        
        // 1. Test 8 errors (s=8, e=0). 2*8 + 0 = 16 <= 16
        let mut corrupted = codeword.clone();
        for i in 0..8 { corrupted[i * 5] ^= 0x55; }
        assert!(rs_decode_64_48(&mut corrupted, &[]).is_ok());
        assert_eq!(corrupted, codeword);
        
        // 2. Test 16 erasures (s=0, e=16). 2*0 + 16 = 16 <= 16
        let mut corrupted = codeword.clone();
        let mut erasures = Vec::new();
        for i in 0..16 {
            corrupted[i * 3] ^= 0x55;
            erasures.push(i * 3);
        }
        assert!(rs_decode_64_48(&mut corrupted, &erasures).is_ok());
        assert_eq!(corrupted, codeword);
        
        // 3. Test 17 erasures -> deterministically fails
        let mut corrupted = codeword.clone();
        let mut erasures = Vec::new();
        for i in 0..17 {
            corrupted[i * 2] ^= 0x55;
            erasures.push(i * 2);
        }
        assert!(rs_decode_64_48(&mut corrupted, &erasures).is_err());
        
        // 4. Test 16 erasures + 1 error -> RS decoder mathematically must find a new valid codeword,
        // so it returns Ok, but the output is a DIFFERENT codeword (rejected later by CRC).
        let mut corrupted = codeword.clone();
        let mut erasures = Vec::new();
        for i in 0..16 {
            corrupted[i * 2] ^= 0x55;
            erasures.push(i * 2);
        }
        corrupted[50] ^= 0xAA;
        assert!(rs_decode_64_48(&mut corrupted, &erasures).is_ok());
        assert_ne!(corrupted, codeword);
    }
}
