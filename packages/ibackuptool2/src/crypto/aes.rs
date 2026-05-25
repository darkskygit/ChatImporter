use aes::cipher::{
    block_padding::NoPadding, BlockCipherDecrypt, BlockModeDecrypt, KeyInit, KeyIvInit,
};
use aes::Aes256;
use log::{trace, warn};

use super::{pack_u64, unpack_64_bit};
use crate::BackupError;

/// perform aes_cbc_256
pub fn decrypt_with_key(key: &Vec<u8>, data: &[u8]) -> Vec<u8> {
    const ZERO_IV: &[u8] = &[0u8; 16];

    let mut out: Vec<u8> = vec![0u8; data.len()];
    out.copy_from_slice(data);
    let is_err = match cbc::Decryptor::<Aes256>::new_from_slices(key.as_slice(), ZERO_IV) {
        Ok(dec) => dec.decrypt_padded::<NoPadding>(&mut out).is_err(),
        Err(_) => true,
    };
    trace!("decrypt: is_err: {}", is_err);
    if is_err {
        out.fill(0);
    }
    out
}

pub fn unwrap_key(kek: &[u8], wpky: &Vec<u8>) -> Result<Vec<u8>, BackupError> {
    trace!("Key: {:x?}", kek);
    trace!("Wrapped: {:x?}", wpky);

    trace!("unwrapping key!");
    let mut c: Vec<u64> = vec![];

    for i in 0..(wpky.len() / 8) {
        let slice: &[u8] = &wpky.as_slice()[i * 8..i * 8 + 8];
        let val = unpack_64_bit(slice);

        if let Some(val) = val {
            c.push(u64::from_be_bytes(val));
        } else {
            return Err(BackupError::InvalidKeybag);
        }
    }

    trace!("C: {:x?}", c);
    if c.len() < 2 {
        return Err(BackupError::InvalidKeybag);
    }

    let n = c.len() - 1;

    trace!("N: {:x?}", n);
    let mut r: Vec<u64> = vec![0; n + 1];

    trace!("R: {:x?}", r);
    let mut a = c[0];

    trace!("A: {:x?}", a);

    //         # Copy C into R, after the first value.
    // for i in xrange(1,n+1):
    //     R[i] = C[i]
    // Copy c into r
    r[1..(n + 1)].copy_from_slice(&c[1..(n + 1)]);

    trace!("key sz: {}", kek.len());
    trace!("c: {:?}", c);
    trace!("n: {:?}", n);
    trace!("r: {:?}", r);
    trace!("a: {:?}", a);

    for j in (0..6).rev() {
        for i in (1..n + 1).rev() {
            trace!("unwrapping key - it a={} n={} j={} i={}", a, n, j, i);
            let val = a ^ ((n as u64) * (j as u64) + (i as u64));
            trace!("a component: {:x?}", val);
            trace!("r[i={}] component: {:x?}", i, r[i]);
            let mut packed = val.to_be_bytes().to_vec();
            trace!("packed component (a): {:x?}", packed);
            let packed2 = r[i].to_be_bytes();
            packed.extend_from_slice(&packed2);
            trace!("packed component: {:x?}", packed);

            trace!("key_length: {}", kek.len() * 8);
            trace!(
                "decrypt(cipher=aes_{}_ecb, kek={}, data={}",
                kek.len() * 8,
                hex::encode(kek),
                hex::encode(packed.as_slice())
            );

            trace!("aes_ecb_256_dec({:x?})", kek);
            let cipher = Aes256::new_from_slice(kek).map_err(|_| BackupError::InvalidKeybag)?;
            if packed.len() != 16 {
                return Err(BackupError::InvalidKeybag);
            }
            let mut block = aes::Block::default();
            block.copy_from_slice(&packed);
            cipher.decrypt_block(&mut block);
            let out = block.as_slice().to_vec();
            trace!("res: {}", hex::encode(&out));

            a = u64::from_be_bytes(
                unpack_64_bit(&out.as_slice()[0..8]).ok_or(BackupError::InvalidKeybag)?,
            );
            trace!("new a: {:x?}", a);
            r[i] = u64::from_be_bytes(
                unpack_64_bit(&out.as_slice()[8..16]).ok_or(BackupError::InvalidKeybag)?,
            );
            trace!("new r[{}]: {:x?}", i, a);
        }
    }

    if a != 0xa6a6a6a6a6a6a6a6 {
        warn!("got iv: 0x{:x}, expected: 0xa6a6a6a6a6a6a6a6", a);
        return Err(BackupError::InvalidPassword);
    }

    let mut result: Vec<u8> = Vec::new();
    trace!("result vector {:x?}", r);
    for (i, value) in r.iter().enumerate().skip(1) {
        // let packed =  Vec::new();
        // let other : Vec<u8> = packed..into();
        let packed = pack_u64(*value);
        trace!("r[i={}] = {:x} == {:x?}", i, value, &packed);

        result.extend_from_slice(&packed);
    }

    trace!("decrypt result: {}", hex::encode(&result));

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypt_with_key_matches_aes_256_cbc_zero_iv_vector() {
        let key = hex::decode("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .unwrap();
        let ciphertext = hex::decode("8ea2b7ca516745bfeafc49904b496089").unwrap();
        let plaintext = hex::decode("00112233445566778899aabbccddeeff").unwrap();

        assert_eq!(decrypt_with_key(&key, &ciphertext), plaintext);
    }

    #[test]
    fn unwrap_key_matches_aes_kw_256_bit_kek_vector() {
        let kek = hex::decode("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .unwrap();
        let wrapped = hex::decode("64e8c3f9ce0f5ba263e9777905818a2a93c8191e7d6e8ae7").unwrap();
        let plaintext = hex::decode("00112233445566778899aabbccddeeff").unwrap();

        assert_eq!(unwrap_key(&kek, &wrapped).unwrap(), plaintext);
    }
}
