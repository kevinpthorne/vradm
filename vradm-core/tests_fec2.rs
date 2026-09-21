#[path = "src/fec.rs"]
mod fec;

fn main() {
    let m = 0x5A5;
    let c = fec::golay_encode(m);
    
    // test some 4-bit errors
    let mut false_accepts = 0;
    for i in 0..24 {
        for j in i+1..24 {
            for k in j+1..24 {
                for l in k+1..24 {
                    let err = (1 << i) | (1 << j) | (1 << k) | (1 << l);
                    let decoded = fec::golay_decode(c ^ err);
                    if decoded.is_ok() {
                        false_accepts += 1;
                    }
                }
            }
        }
    }
    println!("False accepts on 4-bit error: {}", false_accepts);
}
