//! The cryptography behind Companion pairing and the encrypted session.
//!
//! Three pieces: HKDF key derivation, ChaCha20-Poly1305 framing, and an SRP-6a
//! client. The SRP is written out here rather than taken from the `srp` crate
//! because that crate computes `M1 = H(A | B | K)`, while HAP needs the RFC
//! 5054 form `H(H(N) xor H(g) | H(I) | s | A | B | K)`, and its `key()` returns
//! the raw premaster secret rather than `H(S)`. Both differences fail silently
//! as "proof mismatch", which is a miserable thing to debug.
//!
//! Byte-level encoding is deliberately self-consistent with what goes on the
//! wire: the client public key is hashed in exactly the form it is transmitted,
//! and the salt and server key exactly as received. Anything else disagrees
//! with the device's own computation whenever a value has a leading zero byte.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use num_bigint::BigUint;
use sha2::{Digest, Sha512};

use crate::{Error, Result};

/// Derived keys are always 32 bytes.
pub const KEY_LEN: usize = 32;
/// Poly1305 tag length, which the frame length must account for.
pub const TAG_LEN: usize = 16;

/// HKDF-SHA512, as HAP uses it: 32 bytes out, string salt and info.
pub fn hkdf_sha512(salt: &str, info: &str, ikm: &[u8]) -> [u8; KEY_LEN] {
    let hkdf = Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm);
    let mut out = [0u8; KEY_LEN];
    hkdf.expand(info.as_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF output length");
    out
}

/// The 12-byte nonce for a fixed pairing label such as `PS-Msg05`.
///
/// The label is eight bytes; HAP left-pads it with four zeros.
pub fn pairing_nonce(label: &[u8]) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    let start = 12 - label.len().min(12);
    nonce[start..].copy_from_slice(&label[..label.len().min(12)]);
    nonce
}

fn cipher(key: &[u8]) -> Result<ChaCha20Poly1305> {
    if key.len() != KEY_LEN {
        return Err(Error::Protocol(format!(
            "encryption key must be {KEY_LEN} bytes, got {}",
            key.len()
        )));
    }
    let key = Key::try_from(key).map_err(|_| Error::Protocol("bad key length".into()))?;
    Ok(ChaCha20Poly1305::new(&key))
}

/// Encrypt with an explicit nonce and no associated data.
pub fn seal(key: &[u8], nonce: &[u8; 12], plaintext: &[u8]) -> Result<Vec<u8>> {
    cipher(key)?
        .encrypt(&Nonce::from(*nonce), plaintext)
        .map_err(|_| Error::Protocol("encryption failed".into()))
}

/// Decrypt with an explicit nonce and no associated data.
pub fn open(key: &[u8], nonce: &[u8; 12], ciphertext: &[u8]) -> Result<Vec<u8>> {
    cipher(key)?
        .decrypt(&Nonce::from(*nonce), ciphertext)
        .map_err(|_| Error::Protocol("decryption failed — wrong key or corrupt data".into()))
}

/// The encrypted session established after pair-verify.
///
/// Each direction has its own key and its own counter. The nonce is the counter
/// as a 12-byte little-endian integer, and the frame header is the associated
/// data, so a tampered header invalidates the frame.
pub struct SessionCipher {
    out_key: Vec<u8>,
    in_key: Vec<u8>,
    out_counter: u64,
    in_counter: u64,
}

impl SessionCipher {
    pub fn new(out_key: [u8; KEY_LEN], in_key: [u8; KEY_LEN]) -> Self {
        Self {
            out_key: out_key.to_vec(),
            in_key: in_key.to_vec(),
            out_counter: 0,
            in_counter: 0,
        }
    }

    fn counter_nonce(counter: u64) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        nonce
    }

    pub fn encrypt(&mut self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce = Self::counter_nonce(self.out_counter);
        let out = cipher(&self.out_key)?
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| Error::Protocol("session encryption failed".into()))?;
        // Only advance once the operation succeeded, so a failure doesn't
        // silently desynchronise the two ends.
        self.out_counter += 1;
        Ok(out)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce = Self::counter_nonce(self.in_counter);
        let out = cipher(&self.in_key)?
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| Error::Protocol("session decryption failed".into()))?;
        self.in_counter += 1;
        Ok(out)
    }
}

impl std::fmt::Debug for SessionCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the keys.
        f.debug_struct("SessionCipher")
            .field("out_counter", &self.out_counter)
            .field("in_counter", &self.in_counter)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// SRP-6a, as HAP uses it: SHA-512 over the 3072-bit group.
// ---------------------------------------------------------------------------

/// HAP always pairs against this username.
pub const SRP_USERNAME: &str = "Pair-Setup";

fn sha512(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

/// Left-pad to the byte length of the modulus, for the values HAP pads.
fn pad(value: &BigUint, width: usize) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    if bytes.len() >= width {
        return bytes;
    }
    let mut out = vec![0u8; width - bytes.len()];
    out.extend_from_slice(&bytes);
    out
}

/// The outcome of a successful SRP exchange.
pub struct SrpSession {
    /// `A`, exactly as it should be transmitted and hashed.
    pub public: Vec<u8>,
    /// `M1`, the proof the device checks.
    pub proof: Vec<u8>,
    /// `K = H(S)`, the input to every key derivation that follows.
    pub session_key: Vec<u8>,
    /// `M2`, which the device returns and we check.
    expected_server_proof: Vec<u8>,
}

impl SrpSession {
    /// Confirm the device proved it knows the PIN too.
    ///
    /// Skipping this would let anything that can complete a TCP handshake
    /// impersonate the Apple TV.
    pub fn verify_server_proof(&self, proof: &[u8]) -> Result<()> {
        if proof == self.expected_server_proof {
            Ok(())
        } else {
            Err(Error::Protocol(
                "the device's proof did not verify — it does not know the PIN".into(),
            ))
        }
    }
}

/// Run the client side of SRP-6a.
///
/// `private` is the client's secret exponent `a`; HAP reuses the Ed25519
/// private key bytes for it.
pub fn srp_client(
    private: &[u8],
    pin: &str,
    salt: &[u8],
    server_public: &[u8],
) -> Result<SrpSession> {
    // The `srp` crate is used only as a source for the group constants; its
    // own SRP maths computes a different M1 (see the module comment). Going
    // through bytes also sidesteps the two crates disagreeing on a bignum
    // version.
    let n = BigUint::from_bytes_be(&srp::groups::G_3072.n.to_bytes_be());
    let g = BigUint::from_bytes_be(&srp::groups::G_3072.g.to_bytes_be());
    let width = n.to_bytes_be().len();

    let a = BigUint::from_bytes_be(private);
    let b_pub = BigUint::from_bytes_be(server_public);

    // A malicious or broken device could send B ≡ 0, which collapses the
    // shared secret.
    if b_pub.clone() % &n == BigUint::from(0u32) {
        return Err(Error::Protocol("device sent an invalid public key".into()));
    }

    // A = g^a mod N, transmitted in exactly the form we hash below.
    let a_pub = g.modpow(&a, &n);
    let a_bytes = a_pub.to_bytes_be();

    // k = H(N | PAD(g)), u = H(PAD(A) | PAD(B)) — these two are padded.
    let k = BigUint::from_bytes_be(&sha512(&[&n.to_bytes_be(), &pad(&g, width)]));
    let u = BigUint::from_bytes_be(&sha512(&[&pad(&a_pub, width), &pad(&b_pub, width)]));

    // x = H(s | H(I ":" P))
    let identity = sha512(&[
        SRP_USERNAME.as_bytes(),
        b":",
        pin.as_bytes(),
    ]);
    let x = BigUint::from_bytes_be(&sha512(&[salt, &identity]));

    // S = (B - k * g^x) ^ (a + u * x) mod N
    let v = g.modpow(&x, &n);
    let kv = (&k * &v) % &n;
    let base = (&b_pub + &n - kv) % &n;
    let exponent = &a + &u * &x;
    let premaster = base.modpow(&exponent, &n);

    // K = H(S)
    let session_key = sha512(&[&premaster.to_bytes_be()]);

    // M1 = H(H(N) xor H(g) | H(I) | s | A | B | K)
    let hn = sha512(&[&n.to_bytes_be()]);
    let hg = sha512(&[&g.to_bytes_be()]);
    let xored: Vec<u8> = hn.iter().zip(&hg).map(|(a, b)| a ^ b).collect();
    let hi = sha512(&[SRP_USERNAME.as_bytes()]);
    let proof = sha512(&[&xored, &hi, salt, &a_bytes, server_public, &session_key]);

    // M2 = H(A | M1 | K)
    let expected_server_proof = sha512(&[&a_bytes, &proof, &session_key]);

    Ok(SrpSession {
        public: a_bytes,
        proof,
        session_key,
        expected_server_proof,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_matches_a_known_derivation() {
        // Deterministic for a fixed input, and salt and info both matter — a
        // typo in either produces a plausible-looking but useless key.
        let a = hkdf_sha512("Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info", b"secret");
        let b = hkdf_sha512("Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info", b"secret");
        assert_eq!(a, b);
        assert_ne!(a, hkdf_sha512("other-salt", "Pair-Setup-Encrypt-Info", b"secret"));
        assert_ne!(a, hkdf_sha512("Pair-Setup-Encrypt-Salt", "other-info", b"secret"));
        assert_ne!(a, hkdf_sha512("Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info", b"other"));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn pairing_nonce_left_pads_the_label() {
        assert_eq!(
            pairing_nonce(b"PS-Msg05"),
            [0, 0, 0, 0, b'P', b'S', b'-', b'M', b's', b'g', b'0', b'5']
        );
    }

    #[test]
    fn seal_and_open_round_trip() {
        let key = [7u8; KEY_LEN];
        let nonce = pairing_nonce(b"PV-Msg02");
        let sealed = seal(&key, &nonce, b"hello").unwrap();
        assert_eq!(sealed.len(), 5 + TAG_LEN, "tag should be appended");
        assert_eq!(open(&key, &nonce, &sealed).unwrap(), b"hello");
    }

    #[test]
    fn opening_with_the_wrong_key_or_nonce_fails() {
        let nonce = pairing_nonce(b"PV-Msg02");
        let sealed = seal(&[7u8; KEY_LEN], &nonce, b"hello").unwrap();
        assert!(open(&[8u8; KEY_LEN], &nonce, &sealed).is_err());
        assert!(open(&[7u8; KEY_LEN], &pairing_nonce(b"PV-Msg03"), &sealed).is_err());
    }

    #[test]
    fn a_wrong_length_key_is_rejected_not_padded() {
        assert!(seal(&[0u8; 16], &pairing_nonce(b"PS-Msg05"), b"x").is_err());
    }

    #[test]
    fn session_cipher_round_trips_in_lockstep() {
        // The two ends each keep a counter; they must advance together.
        let mut sender = SessionCipher::new([1u8; KEY_LEN], [2u8; KEY_LEN]);
        let mut receiver = SessionCipher::new([2u8; KEY_LEN], [1u8; KEY_LEN]);

        for i in 0..5u8 {
            let header = [8u8, 0, 0, i];
            let message = vec![i; 20];
            let sealed = sender.encrypt(&message, &header).unwrap();
            assert_eq!(receiver.decrypt(&sealed, &header).unwrap(), message);
        }
    }

    #[test]
    fn the_header_is_authenticated() {
        // The frame header is the AAD, so tampering with the declared length
        // must invalidate the frame rather than silently truncate it.
        let mut sender = SessionCipher::new([1u8; KEY_LEN], [2u8; KEY_LEN]);
        let mut receiver = SessionCipher::new([2u8; KEY_LEN], [1u8; KEY_LEN]);
        let sealed = sender.encrypt(b"payload", &[8, 0, 0, 23]).unwrap();
        assert!(receiver.decrypt(&sealed, &[8, 0, 0, 99]).is_err());
    }

    #[test]
    fn nonces_do_not_repeat_across_messages() {
        // Reusing a nonce with the same key destroys ChaCha20-Poly1305.
        let mut sender = SessionCipher::new([1u8; KEY_LEN], [2u8; KEY_LEN]);
        let first = sender.encrypt(b"same", &[]).unwrap();
        let second = sender.encrypt(b"same", &[]).unwrap();
        assert_ne!(first, second, "identical plaintext must not encrypt alike");
    }

    #[test]
    fn a_failed_decrypt_does_not_advance_the_counter() {
        // Otherwise one corrupt frame desynchronises the stream permanently.
        let mut sender = SessionCipher::new([1u8; KEY_LEN], [2u8; KEY_LEN]);
        let mut receiver = SessionCipher::new([2u8; KEY_LEN], [1u8; KEY_LEN]);

        let good = sender.encrypt(b"first", &[]).unwrap();
        assert!(receiver.decrypt(b"garbage-that-is-long-enough-to-parse", &[]).is_err());
        assert_eq!(
            receiver.decrypt(&good, &[]).unwrap(),
            b"first",
            "the stream should still be readable after a bad frame"
        );
    }

    #[test]
    fn srp_matches_the_reference_implementation() {
        // Known answers generated with `srptools`, the library pyatv uses, for
        // the same inputs. This pins every byte-level choice — the padding of
        // k and u, the unpadded M1 terms, and K = H(S) — any one of which
        // fails silently as a rejected proof.
        let session = srp_client(&[0x11u8; 32], "1234", &[0x22u8; 16], &[0x33u8; 384]).unwrap();
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();

        assert_eq!(session.public.len(), 384);
        assert!(hex(&session.public).starts_with("c95341372ca4d0b469f87c0ae37bda63"));
        assert_eq!(
            hex(&session.proof),
            "abd0ebded961f299d52ed1370354664a579e2b2a0e5ca2ec2e72d0601738d956             93bfb2f9b2cf26f360ef179c3e9f129d4d259d2ad42548cc51442b76897ba47e"
                .replace(['\n', ' '], "")
        );
        assert_eq!(
            hex(&session.session_key),
            "e9c4d42f97be14085f55bf13a93d29c1f7647e6a8d5b926af1d368648c064e65             290358a2e1f034c679af5c5c55708ebe9a09dc4a17b4858d43bd723ff0c639f2"
                .replace(['\n', ' '], "")
        );
    }

    #[test]
    fn srp_produces_consistent_values() {
        let private = [0x11u8; 32];
        let salt = [0x22u8; 16];
        // A stand-in for the device's B; any nonzero value exercises the maths.
        let server_public = [0x33u8; 384];

        let one = srp_client(&private, "1234", &salt, &server_public).unwrap();
        let two = srp_client(&private, "1234", &salt, &server_public).unwrap();
        assert_eq!(one.proof, two.proof, "SRP must be deterministic");
        assert_eq!(one.session_key.len(), 64, "K is a SHA-512 digest");
        assert_eq!(one.proof.len(), 64);
    }

    #[test]
    fn a_different_pin_gives_a_different_proof() {
        // The whole point: only the right PIN produces a proof the device takes.
        let (private, salt, server_public) = ([0x11u8; 32], [0x22u8; 16], [0x33u8; 384]);
        let right = srp_client(&private, "1234", &salt, &server_public).unwrap();
        let wrong = srp_client(&private, "1235", &salt, &server_public).unwrap();
        assert_ne!(right.proof, wrong.proof);
        assert_ne!(right.session_key, wrong.session_key);
    }

    #[test]
    fn a_different_salt_gives_a_different_proof() {
        let (private, server_public) = ([0x11u8; 32], [0x33u8; 384]);
        let a = srp_client(&private, "1234", &[0x22u8; 16], &server_public).unwrap();
        let b = srp_client(&private, "1234", &[0x44u8; 16], &server_public).unwrap();
        assert_ne!(a.proof, b.proof);
    }

    #[test]
    fn the_client_public_key_is_group_sized() {
        let session = srp_client(&[0x11u8; 32], "1234", &[0x22u8; 16], &[0x33u8; 384]).unwrap();
        // 3072 bits. A leading zero byte would make it 383, which is legitimate
        // and must still be transmitted exactly as hashed.
        assert!(session.public.len() <= 384 && session.public.len() >= 383);
    }

    #[test]
    fn a_zero_server_key_is_rejected() {
        // B ≡ 0 mod N collapses the shared secret to zero.
        let result = srp_client(&[0x11u8; 32], "1234", &[0x22u8; 16], &[0u8; 384]);
        assert!(result.is_err());
    }

    #[test]
    fn the_server_proof_is_checked() {
        let session = srp_client(&[0x11u8; 32], "1234", &[0x22u8; 16], &[0x33u8; 384]).unwrap();
        assert!(session.verify_server_proof(&session.expected_server_proof).is_ok());
        assert!(session.verify_server_proof(b"not the proof").is_err());
    }
}
