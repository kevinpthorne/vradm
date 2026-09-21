#[path = "src/fec.rs"]
mod fec;

fn main() {
    let m = 0x5A5;
    let c = fec::golay_encode(m);
    
    // test all 1-bit errors
    for i in 0..24 {
        let err = 1 << i;
        assert_eq!(fec::golay_decode(c ^ err).unwrap(), m, "1-bit error at {}", i);
    }
    
    // test all 2-bit errors
    for i in 0..24 {
        for j in i+1..24 {
            let err = (1 << i) | (1 << j);
            assert_eq!(fec::golay_decode(c ^ err).unwrap(), m, "2-bit error at {}, {}", i, j);
        }
    }
    
    // test all 3-bit errors
    for i in 0..24 {
        for j in i+1..24 {
            for k in j+1..24 {
                let err = (1 << i) | (1 << j) | (1 << k);
                let decoded = fec::golay_decode(c ^ err);
                if decoded.is_err() || decoded.unwrap() != m {
                    println!("Failed on 3-bit error: {}, {}, {}", i, j, k);
                    return;
                }
            }
        }
    }
    println!("All 1, 2, 3 bit errors passed.");
}
