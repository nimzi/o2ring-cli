// SPDX-License-Identifier: AGPL-3.0-or-later
//! AES-128-ECB with PKCS#7 padding, as used by newer OxyII firmware once AUTH returns a key
//! (matches `@noble/ciphers` `ecb(key)` in O2Ring Web and the vendor SDK).

use aes::{
    cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit},
    Aes128,
};
use anyhow::{ensure, Result};

pub fn encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - data.len() % 16;
    let mut out = data.to_vec();
    out.extend(std::iter::repeat_n(pad as u8, pad));
    for block in out.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    out
}

pub fn decrypt(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    ensure!(!data.is_empty() && data.len().is_multiple_of(16), "ciphertext length {} isn't a multiple of 16", data.len());
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    let pad = *out.last().unwrap() as usize;
    ensure!(
        (1..=16).contains(&pad) && out[out.len() - pad..].iter().all(|&b| b as usize == pad),
        "bad PKCS#7 padding (wrong key?)"
    );
    out.truncate(out.len() - pad);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fips197_block_and_round_trip() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let pt: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
        let ct = encrypt(&key, &pt);
        // First block is the FIPS-197 C.1 vector; second block is the full padding block.
        assert_eq!(
            &ct[..16],
            &[0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a]
        );
        assert_eq!(ct.len(), 32);
        assert_eq!(decrypt(&key, &ct).unwrap(), pt);
        for n in [0, 1, 4, 15, 20, 512] {
            let msg: Vec<u8> = (0..n).map(|i| i as u8).collect();
            assert_eq!(decrypt(&key, &encrypt(&key, &msg)).unwrap(), msg);
        }
    }
}
