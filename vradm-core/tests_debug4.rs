#[path = "src/fec.rs"]
mod fec;

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 { return 0; }
    fec::EXP[fec::LOG[a as usize] + fec::LOG[b as usize]]
}

fn div(a: u8, b: u8) -> u8 {
    if a == 0 { return 0; }
    fec::EXP[fec::LOG[a as usize] + 255 - fec::LOG[b as usize]]
}

fn rs_decode_original(recv: &mut [u8], erasures: &[usize], n: usize, k: usize) -> Result<(), ()> {
    let t2 = n - k;
    let mut syn = vec![0u8; t2];
    let mut has_error = false;
    for j in 0..t2 {
        let root = fec::EXP[j];
        let mut sum = 0u8;
        for i in 0..n {
            sum = mul(sum, root) ^ recv[i];
        }
        syn[j] = sum;
        if sum != 0 { has_error = true; }
    }
    if !has_error { return Ok(()); }
    
    let mut lambda = vec![0u8; erasures.len() + 1];
    lambda[0] = 1;
    let mut lambda_deg = 0;
    for &pos in erasures {
        let x_j = fec::EXP[(n - 1 - pos) % 255];
        let mut next = vec![0u8; lambda_deg + 2];
        for i in 0..=lambda_deg {
            next[i] ^= lambda[i];
            next[i + 1] ^= mul(lambda[i], x_j);
        }
        lambda = next;
        lambda_deg += 1;
    }
    
    let mut t_syn = vec![0u8; t2];
    for i in 0..t2 {
        let mut sum = 0u8;
        for j in 0..=lambda_deg {
            if i >= j {
                sum ^= mul(lambda[j], syn[i - j]);
            }
        }
        t_syn[i] = sum;
    }
    
    let mut sigma = vec![0u8; t2 + 1];
    sigma[0] = 1;
    let mut b = vec![0u8; t2 + 1];
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
            let mut next_sigma = sigma.clone();
            for j in 0..=t2 {
                if j >= m {
                    next_sigma[j] ^= mul(d, b[j - m]);
                }
            }
            if 2 * l <= i - lambda_deg {
                b = sigma.clone();
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
    
    let mut phi = vec![0u8; t2 + 1];
    for i in 0..=lambda_deg {
        for j in 0..=l {
            if i + j <= t2 {
                phi[i + j] ^= mul(lambda[i], sigma[j]);
            }
        }
    }
    let deg_phi = lambda_deg + l;
    
    let mut locs = vec![];
    for i in 0..n {
        let x_inv = fec::EXP[255 - (n - 1 - i) % 255];
        let mut sum = 0u8;
        let mut x_pow = 1u8;
        for j in 0..=deg_phi {
            sum ^= mul(phi[j], x_pow);
            x_pow = mul(x_pow, x_inv);
        }
        if sum == 0 {
            locs.push(i);
        }
    }
    
    if locs.len() != deg_phi {
        return Err(());
    }
    
    let mut omega = vec![0u8; t2];
    for i in 0..t2 {
        let mut sum = 0u8;
        for j in 0..=deg_phi {
            if i >= j {
                sum ^= mul(phi[j], syn[i - j]);
            }
        }
        omega[i] = sum;
    }
    
    for &pos in &locs {
        let x_inv = fec::EXP[255 - (n - 1 - pos) % 255];
        let mut phi_prime = 0u8;
        let mut x_pow = 1u8;
        for j in (1..=deg_phi).step_by(2) {
            phi_prime ^= mul(phi[j], x_pow);
            x_pow = mul(x_pow, mul(x_inv, x_inv));
        }
        if phi_prime == 0 { return Err(()); }
        
        let mut o_val = 0u8;
        let mut x_pow_o = 1u8;
        for j in 0..deg_phi {
            o_val ^= mul(omega[j], x_pow_o);
            x_pow_o = mul(x_pow_o, x_inv);
        }
        
        let x_j = fec::EXP[(n - 1 - pos) % 255];
        let mag = mul(x_j, div(o_val, phi_prime));
        recv[pos] ^= mag;
    }
    
    Ok(())
}

fn main() {
    let info_bytes: [u8; 48] = [
        0x56, 0x52, 0x41, 0x44, 0x4D, 0x5F, 0x54, 0x45, 0x53, 0x54, 0x5F, 0x46, 0x52, 0x41, 0x4D, 0x45,
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
    ];
    let codeword = fec::rs_encode_64_48(&info_bytes);
    let mut corrupted = codeword.clone();
    let mut erasures = Vec::new();
    for i in 0..15 { // 15 erasures
        corrupted[i * 2] ^= 0x55;
        erasures.push(i * 2);
    }
    corrupted[50] ^= 0xAA; // 1 extra error
    
    let res = rs_decode_original(&mut corrupted, &erasures, 64, 48);
    println!("Original decode result 15+1: {:?}", res);
    println!("Equal to codeword? {}", corrupted == codeword);
}
