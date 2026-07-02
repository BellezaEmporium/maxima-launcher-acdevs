use std::{num::Wrapping, str};

use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyInit, block_padding::Pkcs7};
use chrono::Datelike;
use hex;

type Aes128EcbEnc = ecb::Encryptor<aes::Aes128>;
type Aes128EcbDec = ecb::Decryptor<aes::Aes128>;

const CRYPTO_KEY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

const PRIME_10K: u32 = 104729;
const PRIME_20K: u32 = 224737;
const PRIME_30K: u32 = 350377;

/// Encrypt data with AES-128-ECB-PKCS7, return hex string.
pub fn simple_encrypt(data: &[u8], key: &[u8; 16]) -> String {
    let ct = Aes128EcbEnc::new(key.into()).encrypt_padded_vec::<Pkcs7>(data);
    hex::encode(ct)
}

/// Decrypt hex-encoded AES-128-ECB-PKCS7 data.
pub fn simple_decrypt(data: &[u8], key: &[u8; 16]) -> String {
    let Ok(ciphertext) = hex::decode(data) else { return String::new() };
    Aes128EcbDec::new(key.into())
        .decrypt_padded_vec::<Pkcs7>(&ciphertext)
        .ok()
        .and_then(|v| String::from_utf8(v).ok())
        .unwrap_or_default()
}

pub fn check_challenge_response(response: &str, challenge: &str) -> bool {
    simple_decrypt(response.as_bytes(), &CRYPTO_KEY).as_bytes() == challenge.as_bytes()
}

pub fn make_challenge_response(challenge: &str) -> String {
    simple_encrypt(challenge.as_bytes(), &CRYPTO_KEY)
}

pub fn make_lsx_key(seed: u16) -> [u8; 16] {
    if seed == 0 {
        return CRYPTO_KEY;
    }

    let mut crand = CRandom::default();
    crand.seed(7);
    let seed = (crand.rand() as u32) + (seed as u32);
    crand.seed(seed);

    let mut result: [u8; 16] = [0; 16];
    for i in 0..16 {
        result[i] = crand.rand() as u8;
    }
    result
}

#[derive(Default)]
struct CRandom {
    seed: Wrapping<u32>,
}

impl CRandom {
    fn seed(&mut self, seed: u32) {
        self.seed = Wrapping(seed);
    }

    fn rand(&mut self) -> i32 {
        self.seed = self.seed * Wrapping(214013) + Wrapping(2531011);
        ((self.seed.0 >> 16) & 0xFFFF) as i32
    }
}

/// This code is required to launch games and changes daily
pub fn rtp_handshake() -> u32 {
    let current_date = chrono::Utc::now();

    let time = (PRIME_10K * current_date.year() as u32)
        ^ (current_date.month() * PRIME_20K)
        ^ (current_date.day() * PRIME_30K);
    time ^ (time << 16) ^ (time >> 16)
}
