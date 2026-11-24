use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};

use crate::Fault;

pub(crate) fn random<const N: usize>() -> Result<[u8; N], Fault> {
    let mut result = [0; N];
    getrandom::fill(&mut result).map_err(|_| Fault::Internal)?;
    Ok(result)
}

fn cipher(key: &[u8; 32]) -> Result<LessSafeKey, Fault> {
    UnboundKey::new(&AES_256_GCM, key)
        .map(LessSafeKey::new)
        .map_err(|_| Fault::Internal)
}

pub(crate) fn seal(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, Fault> {
    let nonce = random::<NONCE_LEN>()?;
    let mut envelope = Vec::with_capacity(NONCE_LEN + plaintext.len() + AES_256_GCM.tag_len());
    envelope.extend_from_slice(&nonce);
    let mut payload = plaintext.to_vec();
    cipher(key)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut payload,
        )
        .map_err(|_| Fault::Internal)?;
    envelope.extend_from_slice(&payload);
    Ok(envelope)
}

pub(crate) fn open(key: &[u8; 32], envelope: &[u8], aad: &[u8]) -> Result<Vec<u8>, Fault> {
    let (nonce, ciphertext) = envelope
        .split_first_chunk::<NONCE_LEN>()
        .ok_or(Fault::Ciphertext)?;
    let mut plaintext = ciphertext.to_vec();
    let len = cipher(key)?
        .open_in_place(
            Nonce::assume_unique_for_key(*nonce),
            Aad::from(aad),
            &mut plaintext,
        )
        .map_err(|_| Fault::Ciphertext)?
        .len();
    plaintext.truncate(len);
    Ok(plaintext)
}
