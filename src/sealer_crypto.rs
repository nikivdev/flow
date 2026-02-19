use anyhow::Result;
use bs58;
use crypto_secretbox::{
    XSalsa20Poly1305,
    aead::{Aead, KeyInit},
};
use rand::RngCore;
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

const SECRET_PREFIX: &str = "sealerSecret_z";
const ID_PREFIX: &str = "sealer_z";

pub fn new_x25519_private_key() -> Vec<u8> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.to_vec()
}

pub fn get_sealer_id(secret: &str) -> Result<String> {
    let secret_raw = secret
        .strip_prefix(SECRET_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("invalid sealer secret prefix"))?;
    let private_bytes = bs58::decode(secret_raw)
        .into_vec()
        .map_err(|e| anyhow::anyhow!("invalid base58 sealer secret: {e}"))?;
    let bytes: [u8; 32] = private_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid sealer secret length"))?;

    let public = PublicKey::from(&StaticSecret::from(bytes)).to_bytes();
    Ok(format!("{}{}", ID_PREFIX, bs58::encode(public).into_string()))
}

pub fn seal(
    message: &[u8],
    sender_secret: &str,
    recipient_id: &str,
    nonce_material: &[u8],
) -> Result<Vec<u8>> {
    let sender_secret = decode_secret(sender_secret)?;
    let recipient_public = decode_id(recipient_id)?;
    let sender_key = StaticSecret::from(sender_secret);
    let recipient_key = PublicKey::from(recipient_public);
    let shared_secret = sender_key.diffie_hellman(&recipient_key).to_bytes();
    let nonce = derive_nonce(nonce_material);
    let cipher = XSalsa20Poly1305::new(&shared_secret.into());
    let ciphertext = cipher
        .encrypt(&nonce.into(), message)
        .map_err(|_| anyhow::anyhow!("failed to seal message"))?;
    Ok(ciphertext)
}

pub fn unseal(
    sealed_message: &[u8],
    recipient_secret: &str,
    sender_id: &str,
    nonce_material: &[u8],
) -> Result<Vec<u8>> {
    let recipient_secret = decode_secret(recipient_secret)?;
    let sender_public = decode_id(sender_id)?;
    let recipient_key = StaticSecret::from(recipient_secret);
    let sender_key = PublicKey::from(sender_public);
    let shared_secret = recipient_key.diffie_hellman(&sender_key).to_bytes();
    let nonce = derive_nonce(nonce_material);
    let cipher = XSalsa20Poly1305::new(&shared_secret.into());
    let plaintext = cipher
        .decrypt(&nonce.into(), sealed_message)
        .map_err(|_| anyhow::anyhow!("failed to unseal message"))?;
    Ok(plaintext)
}

fn decode_secret(value: &str) -> Result<[u8; 32]> {
    let encoded = value
        .strip_prefix(SECRET_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("invalid sealer secret prefix"))?;
    let bytes = bs58::decode(encoded)
        .into_vec()
        .map_err(|e| anyhow::anyhow!("invalid base58 secret: {e}"))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid secret key length"))
}

fn decode_id(value: &str) -> Result<[u8; 32]> {
    let encoded = value
        .strip_prefix(ID_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("invalid sealer id prefix"))?;
    let bytes = bs58::decode(encoded)
        .into_vec()
        .map_err(|e| anyhow::anyhow!("invalid base58 id: {e}"))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid public key length"))
}

fn derive_nonce(nonce_material: &[u8]) -> [u8; 24] {
    let hash = blake3::hash(nonce_material);
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&hash.as_bytes()[..24]);
    nonce
}
