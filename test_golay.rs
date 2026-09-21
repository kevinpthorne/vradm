fn main() {
    let p12: [u16; 12] = [
        0xDC5, 0x6E3, 0xB71, 0x5B9, 0x2DD, 0x16F, 0x8B7, 0xC5B, 0xE2D, 0x717, 0xB8B, 0xFFE
    ];
    let encode = |m: u16| -> u32 {
        let mut p = 0u16;
        for i in 0..12 {
            if (m & (1 << (11 - i))) != 0 {
                p ^= p12[i];
            }
        }
        ((m as u32) << 12) | (p as u32)
    };

    let decode = |c: u32| -> Result<u16, ()> {
        let m_recv = (c >> 12) as u16 & 0xFFF;
        let p_recv = (c & 0xFFF) as u16;
        let p_m = encode(m_recv) as u16 & 0xFFF;
        let s = p_m ^ p_recv;

        if s.count_ones() <= 3 {
            return Ok(m_recv);
        }

        for i in 0..12 {
            if (s ^ p12[i]).count_ones() <= 2 {
                return Ok(m_recv ^ (1 << (11 - i)));
            }
        }

        let mut s2 = 0u16;
        for j in 0..12 {
            if (s & p12[j]).count_ones() % 2 != 0 {
                s2 |= 1 << (11 - j);
            }
        }

        if s2.count_ones() <= 3 {
            return Ok(m_recv ^ s2);
        }

        // We need P12^T rows for the second loop.
        // P12^T row i is the i-th column of P12.
        let mut p12_t = [0u16; 12];
        for i in 0..12 {
            for j in 0..12 {
                if (p12[j] & (1 << (11 - i))) != 0 {
                    p12_t[i] |= 1 << (11 - j);
                }
            }
        }

        for i in 0..12 {
            if (s2 ^ p12_t[i]).count_ones() <= 2 {
                return Ok(m_recv ^ s2 ^ p12_t[i]); // Wait, is this right?
            }
        }

        Err(())
    };

    let c = encode(0x5A5);
    println!("c: {:06X}", c);
    let decoded = decode(0xFA5199).unwrap();
    println!("decoded: {:03X}", decoded);
    
    // Check all up to 3 errors
    let mut errs = 0;
    for e in 0..24 {
        for e2 in e+1..24 {
            for e3 in e2+1..24 {
                let mask = (1<<e) | (1<<e2) | (1<<e3);
                if decode(c ^ mask) != Ok(0x5A5) {
                    errs += 1;
                }
            }
        }
    }
    println!("Failed 3 errors: {}", errs);
}
