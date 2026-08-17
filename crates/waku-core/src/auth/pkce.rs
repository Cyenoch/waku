//! PKCE S256 verifier/challenge pair.

use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::error::AuthError;

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Result<Pkce, AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| AuthError::failed("could not generate PKCE"))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_s256_of_verifier() {
        let pkce = generate_pkce().unwrap();
        let digest = Sha256::digest(pkce.verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(pkce.challenge, expected);
        assert_ne!(pkce.verifier, pkce.challenge);
    }
}
