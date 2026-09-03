//! Pair-setup and pair-verify.
//!
//! **Pair-setup** happens once. The Apple TV shows a four-digit PIN, SRP proves
//! both sides know it, and long-term Ed25519 identities are exchanged under a
//! key derived from the SRP secret. The result is credentials we store forever.
//!
//! **Pair-verify** happens on every connection. An ephemeral X25519 exchange,
//! each side signing the transcript with the long-term key from pairing, yields
//! the session keys that encrypt everything afterwards.
//!
//! The salt and info strings below are load-bearing constants: a typo produces
//! a perfectly valid key that the device simply cannot talk to.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

use crate::appletv::crypto::{self, KEY_LEN};
use crate::appletv::tlv8::{Tag, Tlv};
use crate::{Error, Result};

/// Derives the key that encrypts the identity exchange in pair-setup.
const SETUP_ENCRYPT_SALT: &str = "Pair-Setup-Encrypt-Salt";
const SETUP_ENCRYPT_INFO: &str = "Pair-Setup-Encrypt-Info";
/// Derives the value the controller signs to prove it holds the SRP secret.
const SETUP_SIGN_SALT: &str = "Pair-Setup-Controller-Sign-Salt";
const SETUP_SIGN_INFO: &str = "Pair-Setup-Controller-Sign-Info";
/// Derives the key protecting the pair-verify exchange.
const VERIFY_ENCRYPT_SALT: &str = "Pair-Verify-Encrypt-Salt";
const VERIFY_ENCRYPT_INFO: &str = "Pair-Verify-Encrypt-Info";
/// Derives the two session keys. Companion passes an empty salt here.
const SESSION_SALT: &str = "";
const SESSION_OUTPUT_INFO: &str = "ClientEncrypt-main";
const SESSION_INPUT_INFO: &str = "ServerEncrypt-main";

/// What pairing earns us, stored and reused for every later connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The device's long-term public key.
    pub device_public: Vec<u8>,
    /// Our long-term private key.
    pub client_private: Vec<u8>,
    /// The device's pairing identifier.
    pub device_id: Vec<u8>,
    /// The identifier we chose and gave the device.
    pub client_id: Vec<u8>,
}

impl Credentials {
    pub fn to_hex(&self) -> (String, String, String, String) {
        (
            hex(&self.device_public),
            hex(&self.client_private),
            hex(&self.device_id),
            hex(&self.client_id),
        )
    }

    pub fn from_hex(
        device_public: &str,
        client_private: &str,
        device_id: &str,
        client_id: &str,
    ) -> Result<Self> {
        Ok(Self {
            device_public: unhex(device_public)?,
            client_private: unhex(client_private)?,
            device_id: unhex(device_id)?,
            client_id: unhex(client_id)?,
        })
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(Error::Config("stored credential has odd length".into()));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&text[i..i + 2], 16)
                .map_err(|_| Error::Config("stored credential is not hex".into()))
        })
        .collect()
}

fn signing_key(private: &[u8]) -> Result<SigningKey> {
    let bytes: [u8; 32] = private
        .try_into()
        .map_err(|_| Error::Protocol("signing key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

// ---------------------------------------------------------------------------
// Pair-setup
// ---------------------------------------------------------------------------

/// The state carried between the three round trips of pair-setup.
pub struct PairSetup {
    signing: SigningKey,
    client_id: Vec<u8>,
    session_key: Option<[u8; KEY_LEN]>,
    srp: Option<crypto::SrpSession>,
    /// The device's M2, held while we wait for someone to read the PIN off
    /// the screen.
    m2: Option<Tlv>,
}

impl PairSetup {
    /// Start pairing with a fresh identity.
    ///
    /// `client_id` is how the device will remember this remote; a UUID string.
    pub fn new(seed: [u8; 32], client_id: String) -> Self {
        Self {
            signing: SigningKey::from_bytes(&seed),
            client_id: client_id.into_bytes(),
            session_key: None,
            srp: None,
            m2: None,
        }
    }

    /// Stash the device's M2 until the PIN arrives.
    pub fn accept_m2(&mut self, m2: Tlv) {
        self.m2 = Some(m2);
    }

    pub fn pending_m2(&self) -> Result<Tlv> {
        self.m2
            .clone()
            .ok_or_else(|| Error::Protocol("pairing has not been started".into()))
    }

    /// M1 — ask the device to start pairing and show its PIN.
    pub fn start(&self) -> Tlv {
        Tlv::new()
            .set_byte(Tag::Method, 0x00)
            .set_byte(Tag::State, 0x01)
    }

    /// M3 — prove we know the PIN.
    ///
    /// Consumes the device's salt and public key from M2.
    pub fn prove_pin(&mut self, m2: &Tlv, pin: &str) -> Result<Tlv> {
        if let Some(error) = m2.error() {
            return Err(Error::Protocol(format!("pairing refused: {error}")));
        }
        let salt = m2.require(Tag::Salt)?;
        let device_public = m2.require(Tag::PublicKey)?;

        // HAP reuses the Ed25519 private key bytes as the SRP secret exponent.
        let srp = crypto::srp_client(&self.signing.to_bytes(), pin, salt, device_public)?;
        let tlv = Tlv::new()
            .set_byte(Tag::State, 0x03)
            .set(Tag::PublicKey, srp.public.clone())
            .set(Tag::Proof, srp.proof.clone());
        self.srp = Some(srp);
        Ok(tlv)
    }

    /// M5 — hand over our long-term identity, encrypted under the SRP secret.
    ///
    /// Checks the device's own proof first: without that, anything that can
    /// accept a TCP connection could pose as the Apple TV.
    pub fn exchange_identity(&mut self, m4: &Tlv) -> Result<Tlv> {
        if let Some(error) = m4.error() {
            return Err(Error::Protocol(format!("PIN rejected: {error}")));
        }
        let srp = self
            .srp
            .as_ref()
            .ok_or_else(|| Error::Protocol("pair-setup is out of order".into()))?;
        srp.verify_server_proof(m4.require(Tag::Proof)?)?;

        let sign_key = crypto::hkdf_sha512(SETUP_SIGN_SALT, SETUP_SIGN_INFO, &srp.session_key);
        let session_key =
            crypto::hkdf_sha512(SETUP_ENCRYPT_SALT, SETUP_ENCRYPT_INFO, &srp.session_key);
        self.session_key = Some(session_key);

        let public = self.signing.verifying_key().to_bytes();
        // The transcript proves this identity belongs to whoever knew the PIN.
        let mut info = sign_key.to_vec();
        info.extend_from_slice(&self.client_id);
        info.extend_from_slice(&public);
        let signature = self.signing.sign(&info);

        let inner = Tlv::new()
            .set(Tag::Identifier, self.client_id.clone())
            .set(Tag::PublicKey, public.to_vec())
            .set(Tag::Signature, signature.to_bytes().to_vec())
            .encode();

        let sealed = crypto::seal(
            &session_key,
            &crypto::pairing_nonce(b"PS-Msg05"),
            &inner,
        )?;
        Ok(Tlv::new()
            .set_byte(Tag::State, 0x05)
            .set(Tag::EncryptedData, sealed))
    }

    /// M6 — read back the device's identity and produce credentials.
    pub fn finish(&self, m6: &Tlv) -> Result<Credentials> {
        if let Some(error) = m6.error() {
            return Err(Error::Protocol(format!("pairing failed: {error}")));
        }
        let session_key = self
            .session_key
            .ok_or_else(|| Error::Protocol("pair-setup is out of order".into()))?;

        let plaintext = crypto::open(
            &session_key,
            &crypto::pairing_nonce(b"PS-Msg06"),
            m6.require(Tag::EncryptedData)?,
        )?;
        let inner = Tlv::decode(&plaintext)?;

        Ok(Credentials {
            device_public: inner.require(Tag::PublicKey)?.to_vec(),
            client_private: self.signing.to_bytes().to_vec(),
            device_id: inner.require(Tag::Identifier)?.to_vec(),
            client_id: self.client_id.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Pair-verify
// ---------------------------------------------------------------------------

/// The ephemeral half of a connection: proves both identities, derives session
/// keys, and is then discarded.
pub struct PairVerify {
    secret: StaticSecret,
    public: X25519Public,
    credentials: Credentials,
    shared: Option<[u8; 32]>,
}

impl PairVerify {
    pub fn new(seed: [u8; 32], credentials: Credentials) -> Self {
        let secret = StaticSecret::from(seed);
        let public = X25519Public::from(&secret);
        Self {
            secret,
            public,
            credentials,
            shared: None,
        }
    }

    /// M1 — offer our ephemeral public key.
    pub fn start(&self) -> Tlv {
        Tlv::new()
            .set_byte(Tag::State, 0x01)
            .set(Tag::PublicKey, self.public.to_bytes().to_vec())
    }

    /// M3 — check the device's signature and return ours.
    pub fn respond(&mut self, m2: &Tlv) -> Result<Tlv> {
        if let Some(error) = m2.error() {
            return Err(Error::Protocol(format!("verification refused: {error}")));
        }
        let device_public = m2.require(Tag::PublicKey)?;
        let encrypted = m2.require(Tag::EncryptedData)?;

        let peer: [u8; 32] = device_public
            .try_into()
            .map_err(|_| Error::Protocol("device sent a malformed public key".into()))?;
        let shared = self.secret.diffie_hellman(&X25519Public::from(peer)).to_bytes();

        let session_key =
            crypto::hkdf_sha512(VERIFY_ENCRYPT_SALT, VERIFY_ENCRYPT_INFO, &shared);
        let plaintext = crypto::open(
            &session_key,
            &crypto::pairing_nonce(b"PV-Msg02"),
            encrypted,
        )?;
        let inner = Tlv::decode(&plaintext)?;

        let identifier = inner.require(Tag::Identifier)?;
        if identifier != self.credentials.device_id {
            return Err(Error::Protocol(
                "the device identified itself as a different Apple TV".into(),
            ));
        }

        // Verify it holds the long-term key we paired with. Skipping this would
        // let anything on the network impersonate the Apple TV.
        let device_key: [u8; 32] = self
            .credentials
            .device_public
            .as_slice()
            .try_into()
            .map_err(|_| Error::Config("stored device key is malformed".into()))?;
        let verifying = VerifyingKey::from_bytes(&device_key)
            .map_err(|_| Error::Config("stored device key is not a valid key".into()))?;

        let mut transcript = device_public.to_vec();
        transcript.extend_from_slice(identifier);
        transcript.extend_from_slice(&self.public.to_bytes());

        let signature_bytes: [u8; 64] = inner
            .require(Tag::Signature)?
            .try_into()
            .map_err(|_| Error::Protocol("device signature has the wrong length".into()))?;
        verifying
            .verify(&transcript, &Signature::from_bytes(&signature_bytes))
            .map_err(|_| {
                Error::Protocol("the device's signature did not verify — re-pair".into())
            })?;

        // Sign the mirrored transcript so the device can verify us in turn.
        let signing = signing_key(&self.credentials.client_private)?;
        let mut ours = self.public.to_bytes().to_vec();
        ours.extend_from_slice(&self.credentials.client_id);
        ours.extend_from_slice(device_public);
        let signature = signing.sign(&ours);

        let inner = Tlv::new()
            .set(Tag::Identifier, self.credentials.client_id.clone())
            .set(Tag::Signature, signature.to_bytes().to_vec())
            .encode();
        let sealed = crypto::seal(
            &session_key,
            &crypto::pairing_nonce(b"PV-Msg03"),
            &inner,
        )?;

        self.shared = Some(shared);
        Ok(Tlv::new()
            .set_byte(Tag::State, 0x03)
            .set(Tag::EncryptedData, sealed))
    }

    /// The two session keys, once verification has completed.
    pub fn session_keys(&self) -> Result<([u8; KEY_LEN], [u8; KEY_LEN])> {
        let shared = self
            .shared
            .ok_or_else(|| Error::Protocol("pair-verify has not completed".into()))?;
        Ok((
            crypto::hkdf_sha512(SESSION_SALT, SESSION_OUTPUT_INFO, &shared),
            crypto::hkdf_sha512(SESSION_SALT, SESSION_INPUT_INFO, &shared),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> (Credentials, SigningKey) {
        let device = SigningKey::from_bytes(&[9u8; 32]);
        (
            Credentials {
                device_public: device.verifying_key().to_bytes().to_vec(),
                client_private: [3u8; 32].to_vec(),
                device_id: b"device-id".to_vec(),
                client_id: b"client-id".to_vec(),
            },
            device,
        )
    }

    #[test]
    fn credentials_round_trip_through_hex() {
        let (creds, _) = credentials();
        let (a, b, c, d) = creds.to_hex();
        assert_eq!(Credentials::from_hex(&a, &b, &c, &d).unwrap(), creds);
    }

    #[test]
    fn malformed_stored_credentials_are_rejected() {
        assert!(unhex("abc").is_err(), "odd length");
        assert!(unhex("zz").is_err(), "not hex");
    }

    #[test]
    fn pair_setup_starts_at_state_one() {
        let setup = PairSetup::new([1u8; 32], "client".into());
        let m1 = setup.start();
        assert_eq!(m1.get_byte(Tag::State), Some(1));
        assert_eq!(m1.get_byte(Tag::Method), Some(0));
    }

    #[test]
    fn pair_setup_surfaces_a_rejected_pin() {
        // The device answers M3 with an Authentication error for a wrong PIN;
        // that must read as "wrong PIN", not a generic protocol failure.
        let mut setup = PairSetup::new([1u8; 32], "client".into());
        let m2 = Tlv::new()
            .set(Tag::Salt, vec![1u8; 16])
            .set(Tag::PublicKey, vec![2u8; 384]);
        setup.prove_pin(&m2, "1111").unwrap();

        let m4 = Tlv::new().set_byte(Tag::Error, 0x02);
        let error = setup.exchange_identity(&m4).unwrap_err().to_string();
        assert!(error.contains("PIN"), "got: {error}");
    }

    #[test]
    fn pair_setup_rejects_a_device_that_cannot_prove_the_pin() {
        // A device returning a bogus proof must not be trusted with our
        // long-term identity.
        let mut setup = PairSetup::new([1u8; 32], "client".into());
        let m2 = Tlv::new()
            .set(Tag::Salt, vec![1u8; 16])
            .set(Tag::PublicKey, vec![2u8; 384]);
        setup.prove_pin(&m2, "1234").unwrap();

        let m4 = Tlv::new().set(Tag::Proof, vec![0u8; 64]);
        assert!(setup.exchange_identity(&m4).is_err());
    }

    #[test]
    fn pair_setup_refuses_to_run_out_of_order() {
        let mut setup = PairSetup::new([1u8; 32], "client".into());
        assert!(setup.exchange_identity(&Tlv::new()).is_err());
        assert!(setup.finish(&Tlv::new()).is_err());
    }

    #[test]
    fn pair_setup_reports_missing_fields() {
        let mut setup = PairSetup::new([1u8; 32], "client".into());
        let error = setup
            .prove_pin(&Tlv::new().set(Tag::Salt, vec![1u8; 16]), "1234")
            .unwrap_err()
            .to_string();
        assert!(error.contains("PublicKey"), "got: {error}");
    }

    /// Play the device's side of pair-verify, so the whole exchange is testable
    /// without an Apple TV.
    fn device_m2(
        device: &SigningKey,
        device_id: &[u8],
        client_public: &[u8],
    ) -> (Tlv, [u8; 32]) {
        let secret = StaticSecret::from([5u8; 32]);
        let public = X25519Public::from(&secret);
        let peer: [u8; 32] = client_public.try_into().unwrap();
        let shared = secret.diffie_hellman(&X25519Public::from(peer)).to_bytes();

        let mut transcript = public.to_bytes().to_vec();
        transcript.extend_from_slice(device_id);
        transcript.extend_from_slice(client_public);
        let signature = device.sign(&transcript);

        let inner = Tlv::new()
            .set(Tag::Identifier, device_id.to_vec())
            .set(Tag::Signature, signature.to_bytes().to_vec())
            .encode();
        let key = crypto::hkdf_sha512(VERIFY_ENCRYPT_SALT, VERIFY_ENCRYPT_INFO, &shared);
        let sealed =
            crypto::seal(&key, &crypto::pairing_nonce(b"PV-Msg02"), &inner).unwrap();

        (
            Tlv::new()
                .set_byte(Tag::State, 0x02)
                .set(Tag::PublicKey, public.to_bytes().to_vec())
                .set(Tag::EncryptedData, sealed),
            shared,
        )
    }

    #[test]
    fn pair_verify_completes_against_a_well_behaved_device() {
        let (creds, device) = credentials();
        let mut verify = PairVerify::new([4u8; 32], creds.clone());
        let client_public = verify.start();
        let client_public = client_public.get(Tag::PublicKey).unwrap().to_vec();

        let (m2, device_shared) = device_m2(&device, &creds.device_id, &client_public);
        let m3 = verify.respond(&m2).expect("verification should succeed");
        assert_eq!(m3.get_byte(Tag::State), Some(3));
        assert!(m3.get(Tag::EncryptedData).is_some());

        // Both sides must land on the same session keys.
        let (out_key, in_key) = verify.session_keys().unwrap();
        assert_eq!(
            out_key,
            crypto::hkdf_sha512(SESSION_SALT, SESSION_OUTPUT_INFO, &device_shared)
        );
        assert_eq!(
            in_key,
            crypto::hkdf_sha512(SESSION_SALT, SESSION_INPUT_INFO, &device_shared)
        );
        assert_ne!(out_key, in_key, "directions must use different keys");
    }

    #[test]
    fn pair_verify_rejects_a_forged_signature() {
        // An impostor with the right identifier but the wrong key must fail.
        let (creds, _real) = credentials();
        let impostor = SigningKey::from_bytes(&[77u8; 32]);
        let mut verify = PairVerify::new([4u8; 32], creds.clone());
        let client_public = verify.start().get(Tag::PublicKey).unwrap().to_vec();

        let (m2, _) = device_m2(&impostor, &creds.device_id, &client_public);
        let error = verify.respond(&m2).unwrap_err().to_string();
        assert!(error.contains("signature") || error.contains("decrypt"), "got: {error}");
    }

    #[test]
    fn pair_verify_rejects_a_different_device() {
        let (creds, device) = credentials();
        let mut verify = PairVerify::new([4u8; 32], creds.clone());
        let client_public = verify.start().get(Tag::PublicKey).unwrap().to_vec();

        let (m2, _) = device_m2(&device, b"someone-else", &client_public);
        let error = verify.respond(&m2).unwrap_err().to_string();
        assert!(error.contains("different Apple TV"), "got: {error}");
    }

    #[test]
    fn session_keys_are_unavailable_before_verification() {
        let (creds, _) = credentials();
        let verify = PairVerify::new([4u8; 32], creds);
        assert!(verify.session_keys().is_err());
    }

    #[test]
    fn pair_verify_rejects_a_malformed_device_key() {
        let (creds, _) = credentials();
        let mut verify = PairVerify::new([4u8; 32], creds);
        let m2 = Tlv::new()
            .set(Tag::PublicKey, vec![1u8; 5])
            .set(Tag::EncryptedData, vec![0u8; 40]);
        assert!(verify.respond(&m2).is_err());
    }
}
