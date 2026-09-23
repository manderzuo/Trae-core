use std::{collections::BTreeMap, env};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM},
    rand::{SecureRandom, SystemRandom},
};
use zeroize::Zeroize;

const KEY_LENGTH: usize = 32;
const NONCE_LENGTH: usize = 12;
const TAG_LENGTH: usize = 16;

struct SecretKey([u8; KEY_LENGTH]);

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub struct EncryptedKeyCopy {
    pub ciphertext: Vec<u8>,
    pub key_version: u32,
}

pub struct KeyVault {
    active_version: u32,
    keys: BTreeMap<u32, SecretKey>,
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum KeyVaultError {
    #[error("key vault configuration is missing or invalid")]
    Configuration,
    #[error("key vault version is unavailable")]
    VersionUnavailable,
    #[error("encrypted key material is invalid or cannot be authenticated")]
    CiphertextInvalid,
    #[error("key encryption operation failed")]
    EncryptionFailed,
}

impl KeyVault {
    pub fn from_env() -> Result<Self, KeyVaultError> {
        let active_key = env::var("STARLINK_ROUTER_KEY_ENCRYPTION_KEY").ok();
        let active_version = env::var("STARLINK_ROUTER_KEY_ENCRYPTION_KEY_VERSION").ok();
        let previous_keys = env::var("STARLINK_ROUTER_KEY_ENCRYPTION_PREVIOUS_KEYS").ok();
        Self::from_configuration(
            active_key.as_deref(),
            active_version.as_deref(),
            previous_keys.as_deref(),
        )
    }

    pub fn from_configuration(
        active_key_base64: Option<&str>,
        active_version: Option<&str>,
        previous_keys_json: Option<&str>,
    ) -> Result<Self, KeyVaultError> {
        let active_key_base64 = active_key_base64
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(KeyVaultError::Configuration)?;
        let mut active_key = decode_key(active_key_base64)?;
        let active_version = match active_version {
            Some(value) => match value.parse::<u32>().ok().filter(|version| *version > 0) {
                Some(version) => version,
                None => {
                    active_key.zeroize();
                    return Err(KeyVaultError::Configuration);
                }
            },
            None => 1,
        };
        let mut previous_keys = BTreeMap::new();
        if let Some(raw) = previous_keys_json.map(str::trim).filter(|value| !value.is_empty()) {
            let encoded_keys: BTreeMap<String, String> = match serde_json::from_str(raw) {
                Ok(keys) => keys,
                Err(_) => {
                    active_key.zeroize();
                    return Err(KeyVaultError::Configuration);
                }
            };
            for (version, encoded_key) in encoded_keys {
                let Some(version) = version.parse::<u32>().ok().filter(|version| *version > 0) else {
                    active_key.zeroize();
                    zeroize_raw_keys(&mut previous_keys);
                    return Err(KeyVaultError::Configuration);
                };
                if version == active_version || previous_keys.contains_key(&version) {
                    active_key.zeroize();
                    zeroize_raw_keys(&mut previous_keys);
                    return Err(KeyVaultError::Configuration);
                }
                match decode_key(&encoded_key) {
                    Ok(key) => { previous_keys.insert(version, key); }
                    Err(error) => {
                        active_key.zeroize();
                        zeroize_raw_keys(&mut previous_keys);
                        return Err(error);
                    }
                }
            }
        }
        Self::from_material(active_version, active_key, previous_keys)
    }

    pub fn from_material(
        active_version: u32,
        mut active_key: [u8; KEY_LENGTH],
        mut previous_keys: BTreeMap<u32, [u8; KEY_LENGTH]>,
    ) -> Result<Self, KeyVaultError> {
        if active_version == 0 || previous_keys.contains_key(&active_version) {
            active_key.zeroize();
            zeroize_raw_keys(&mut previous_keys);
            return Err(KeyVaultError::Configuration);
        }
        if previous_keys.contains_key(&0) {
            active_key.zeroize();
            zeroize_raw_keys(&mut previous_keys);
            return Err(KeyVaultError::Configuration);
        }
        let mut keys = BTreeMap::new();
        keys.insert(active_version, SecretKey(active_key));
        for (version, key) in previous_keys {
            keys.insert(version, SecretKey(key));
        }
        Ok(Self { active_version, keys })
    }

    pub fn for_test() -> Self {
        Self::from_material(1, [0x5a; KEY_LENGTH], BTreeMap::new())
            .expect("static test key material is valid")
    }

    pub fn active_version(&self) -> u32 {
        self.active_version
    }

    pub fn encrypt(&self, key_id: &str, plaintext: &str) -> Result<EncryptedKeyCopy, KeyVaultError> {
        let key = self.aead_key(self.active_version)?;
        let mut nonce_bytes = [0_u8; NONCE_LENGTH];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| KeyVaultError::EncryptionFailed)?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let aad = associated_data(key_id, self.active_version);
        let mut sealed = plaintext.as_bytes().to_vec();
        key.seal_in_place_append_tag(nonce, Aad::from(aad.as_slice()), &mut sealed)
            .map_err(|_| KeyVaultError::EncryptionFailed)?;
        let mut ciphertext = Vec::with_capacity(NONCE_LENGTH + sealed.len());
        ciphertext.extend_from_slice(&nonce_bytes);
        ciphertext.extend_from_slice(&sealed);
        Ok(EncryptedKeyCopy {
            ciphertext,
            key_version: self.active_version,
        })
    }

    pub fn decrypt(
        &self,
        key_id: &str,
        key_version: u32,
        ciphertext: &[u8],
    ) -> Result<String, KeyVaultError> {
        if ciphertext.len() < NONCE_LENGTH + TAG_LENGTH {
            return Err(KeyVaultError::CiphertextInvalid);
        }
        let key = self.aead_key(key_version)?;
        let nonce_bytes: [u8; NONCE_LENGTH] = ciphertext[..NONCE_LENGTH]
            .try_into()
            .map_err(|_| KeyVaultError::CiphertextInvalid)?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let aad = associated_data(key_id, key_version);
        let mut opened = ciphertext[NONCE_LENGTH..].to_vec();
        let plaintext = key
            .open_in_place(nonce, Aad::from(aad.as_slice()), &mut opened)
            .map_err(|_| KeyVaultError::CiphertextInvalid)?;
        let result = String::from_utf8(plaintext.to_vec())
            .map_err(|_| KeyVaultError::CiphertextInvalid);
        opened.zeroize();
        result
    }

    pub fn reencrypt(
        &self,
        key_id: &str,
        old_version: u32,
        ciphertext: &[u8],
    ) -> Result<EncryptedKeyCopy, KeyVaultError> {
        if old_version == self.active_version {
            return Ok(EncryptedKeyCopy {
                ciphertext: ciphertext.to_vec(),
                key_version: self.active_version,
            });
        }
        let mut plaintext = self.decrypt(key_id, old_version, ciphertext)?;
        let encrypted = self.encrypt(key_id, &plaintext);
        plaintext.zeroize();
        encrypted
    }

    fn aead_key(&self, version: u32) -> Result<LessSafeKey, KeyVaultError> {
        let material = self
            .keys
            .get(&version)
            .ok_or(KeyVaultError::VersionUnavailable)?;
        let key = UnboundKey::new(&AES_256_GCM, &material.0)
            .map_err(|_| KeyVaultError::Configuration)?;
        Ok(LessSafeKey::new(key))
    }
}

fn decode_key(encoded: &str) -> Result<[u8; KEY_LENGTH], KeyVaultError> {
    let mut decoded = STANDARD
        .decode(encoded.trim())
        .map_err(|_| KeyVaultError::Configuration)?;
    if decoded.len() != KEY_LENGTH {
        decoded.zeroize();
        return Err(KeyVaultError::Configuration);
    }
    let mut key = [0_u8; KEY_LENGTH];
    key.copy_from_slice(&decoded);
    decoded.zeroize();
    Ok(key)
}

fn zeroize_raw_keys(keys: &mut BTreeMap<u32, [u8; KEY_LENGTH]>) {
    for key in keys.values_mut() {
        key.zeroize();
    }
}

fn associated_data(key_id: &str, version: u32) -> Vec<u8> {
    format!("starlink-dimension-router:api-key-copy:v1:{version}:{key_id}").into_bytes()
}
