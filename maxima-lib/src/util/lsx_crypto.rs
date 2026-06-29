use aes::Aes128;
use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyInit, block_padding::Pkcs7};

use ecb::{Decryptor, Encryptor};
use hex;
use thiserror::Error;

type EcbEnc = Encryptor<Aes128>;
type EcbDec = Decryptor<Aes128>;

const KEY_SIZE: usize = 16;
const DEFAULT_SEED: u32 = 7;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("Input cannot be empty")]
    EmptyInput,
    #[error("Decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("UTF-8 conversion error")]
    Utf8Error(#[from] std::string::FromUtf8Error),
}

#[derive(Clone, Debug)]
struct Random {
    state: u64,
}

impl Random {
    fn new(seed: u32) -> Self {
        Self { state: seed as u64 }
    }
    fn set_seed(&mut self, seed: u32) {
        self.state = seed as u64;
    }
    /// LCG constants from Numerical Recipes / glibc
    fn next(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.state >> 32) as u32
    }
}

#[derive(Clone, Debug)]
pub struct Crypto {
    pub(crate) key: [u8; KEY_SIZE],
    rng: Random,
}

impl Crypto {
    pub fn new(seed: u32) -> Self {
        let mut s = Self {
            key: [0u8; KEY_SIZE],
            rng: Random::new(DEFAULT_SEED),
        };
        s.set_key(seed);
        s
    }

    pub fn set_key(&mut self, seed: u32) {
        if seed == 0 {
            for (i, b) in self.key.iter_mut().enumerate() {
                *b = i as u8;
            }
        } else {
            let new_seed = self.rng.next().wrapping_add(seed);
            self.rng.set_seed(new_seed);
            for b in self.key.iter_mut() {
                *b = self.rng.next() as u8;
            }
        }
    }

    pub fn encrypt(&self, plain: &str) -> Result<Vec<u8>, CryptoError> {
        if plain.is_empty() {
            return Err(CryptoError::EmptyInput);
        }
        Ok(EcbEnc::new(&self.key.into()).encrypt_padded_vec::<Pkcs7>(plain.as_bytes()))
    }

    pub fn decrypt(&self, cipher: &[u8]) -> Result<String, CryptoError> {
        if cipher.is_empty() {
            return Err(CryptoError::EmptyInput);
        }
        let pt = EcbDec::new(&self.key.into())
            .decrypt_padded_vec::<Pkcs7>(cipher)
            .map_err(|e| CryptoError::DecryptionFailed(e.to_string()))?;
        String::from_utf8(pt).map_err(CryptoError::from)
    }

    /// Encrypt `key` with current AES key, hex-encode, derive new seed from first 2 ASCII bytes,
    /// rotate internal key to session key, return hex response.
    pub fn prepare_challenge_response(&mut self, key: &str) -> Result<String, CryptoError> {
        let ct = self.encrypt(key)?;
        let hex_str = hex::encode(ct);
        let b = hex_str.as_bytes();
        let seed = ((b[0] as u32) << 8) | (b[1] as u32);
        self.set_key(seed);
        Ok(hex_str)
    }

    pub fn current_key(&self) -> [u8; 16] {
        self.key
    }
}