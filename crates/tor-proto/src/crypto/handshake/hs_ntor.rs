//! Implements the HS ntor key exchange, as used in v3 onion services.
//!
//! The Ntor protocol of this section is specified in section
//! [NTOR-WITH-EXTRA-DATA] of rend-spec-v3.txt.
//!
//! The main difference between this HS Ntor handshake and the standard Ntor
//! handshake in ./ntor.rs is that this one allows each party to encrypt data
//! (without forward secrecy) after it sends the first message. This
//! opportunistic encryption property is used by clients in the onion service
//! protocol to encrypt introduction data in the INTRODUCE1 cell, and by
//! services to encrypt data in the RENDEZVOUS1 cell.
//!
//! # Status
//!
//! This module is a work in progress, and is not actually used anywhere yet
//! or tested: please expect the API to change.
//!
//! This module is available only when the `hs-common` feature is enabled.
//
// TODO hs: go  through this code carefully and make sure that its APIs and
// behavior are still what we want.

// We want to use the exact variable names from the rend-spec-v3.txt proposal.
// This means that we allow variables to be named x (privkey) and X (pubkey).
#![allow(non_snake_case)]

use crate::crypto::handshake::KeyGenerator;
use crate::crypto::ll::kdf::{Kdf, ShakeKdf};
use crate::{Error, Result};
use tor_bytes::{Reader, SecretBuf, Writer};
use tor_hscrypto::{
    ops::{hs_mac, HS_MAC_LEN},
    pk::{HsIntroPtSessionIdKey, HsSvcNtorKey, HsSvcNtorSecretKey},
    Subcredential,
};
use tor_llcrypto::pk::{curve25519, ed25519};
use tor_llcrypto::util::ct::CtByteArray;
use tor_llcrypto::util::rand_compat::RngCompatExt;

use cipher::{KeyIvInit, StreamCipher};

use generic_array::GenericArray;
use rand_core::{CryptoRng, RngCore};
use tor_error::into_internal;
use tor_llcrypto::cipher::aes::Aes256Ctr;
use zeroize::Zeroizing;

/// The ENC_KEY from the HS Ntor protocol
//
// TODO (nickm): Any move operations applied to this key could subvert the zeroizing.
type EncKey = Zeroizing<[u8; 32]>;
/// The MAC_KEY from the HS Ntor protocol
type MacKey = [u8; 32];
/// A generic 256-bit MAC tag
type MacTag = CtByteArray<HS_MAC_LEN>;
/// The AUTH_INPUT_MAC from the HS Ntor protocol
type AuthInputMac = MacTag;

/// The key generator used by the HS ntor handshake.  Implements the simple key
/// expansion protocol specified in section "Key expansion" of rend-spec-v3.txt .
pub struct HsNtorHkdfKeyGenerator {
    /// Secret data derived from the handshake, used as input to HKDF
    seed: SecretBuf,
}

impl HsNtorHkdfKeyGenerator {
    /// Create a new key generator to expand a given seed
    pub fn new(seed: SecretBuf) -> Self {
        HsNtorHkdfKeyGenerator { seed }
    }
}

impl KeyGenerator for HsNtorHkdfKeyGenerator {
    /// Expand the seed into a keystream of 'keylen' size
    fn expand(self, keylen: usize) -> Result<SecretBuf> {
        ShakeKdf::new().derive(&self.seed[..], keylen)
    }
}

/*********************** Client Side Code ************************************/

/// Information about an onion service that is needed for a client to perform an
/// hs_ntor handshake with it.
#[derive(Clone)]
pub struct HsNtorServiceInfo {
    /// Introduction point encryption key (aka `B`, aka `KP_hss_ntor`)
    /// (found in the HS descriptor)
    B: HsSvcNtorKey,

    /// Introduction point authentication key (aka `AUTH_KEY`, aka `` )
    /// (found in the HS descriptor)
    auth_key: HsIntroPtSessionIdKey,

    /// Service subcredential
    subcredential: Subcredential,
}

impl HsNtorServiceInfo {
    /// Create a new `HsNtorClientInput`
    pub fn new(
        B: HsSvcNtorKey,
        auth_key: HsIntroPtSessionIdKey,
        subcredential: Subcredential,
    ) -> Self {
        HsNtorServiceInfo {
            B,
            auth_key,
            subcredential,
        }
    }
}

/// Client state for an ntor handshake.
pub struct HsNtorClientState {
    /// Information about the service we are connecting to.
    service_info: HsNtorServiceInfo,

    /// The temporary curve25519 secret that we generated for this handshake.
    x: curve25519::StaticSecret,
    /// The corresponding private key
    X: curve25519::PublicKey,
}

/// Encrypt the 'plaintext' using 'enc_key'. Then compute the intro cell MAC
/// using 'mac_key' and return (ciphertext, mac_tag).
fn encrypt_and_mac(
    plaintext: &[u8],
    other_data: &[u8],
    public_key: &[u8], // XXXX terrible design.
    enc_key: &EncKey,
    mac_key: MacKey,
) -> (Vec<u8>, MacTag) {
    let mut ciphertext = plaintext.to_vec();
    // Encrypt the introduction data using 'enc_key'
    let zero_iv = GenericArray::default();
    let mut cipher = Aes256Ctr::new(enc_key.as_ref().into(), &zero_iv);
    cipher.apply_keystream(&mut ciphertext);

    // Now staple the other INTRODUCE1 data right before the ciphertext to
    // create the body of the MAC tag
    let mut mac_body: Vec<u8> = Vec::new();
    mac_body.extend(other_data);
    mac_body.extend(public_key);
    mac_body.extend(&ciphertext);
    let mac_tag = hs_mac(&mac_key, &mac_body);

    (ciphertext, mac_tag)
}

/// The client is about to make an INTRODUCE1 cell. Perform the first part of
/// the client handshake.
///
/// Return a state object containing the current progress of the handshake, and
/// the data that should be written in the INTRODUCE1 cell. The data that is
/// written is:
///
///  CLIENT_PK                [PK_PUBKEY_LEN bytes]
///  ENCRYPTED_DATA           [Padded to length of plaintext]
///  MAC                      [MAC_LEN bytes]
pub fn client_send_intro<R>(
    rng: &mut R,
    service: &HsNtorServiceInfo,
    intro_header: &[u8],
    plaintext_body: &[u8],
) -> Result<(HsNtorClientState, Vec<u8>)>
where
    R: RngCore + CryptoRng,
{
    // Create client's ephemeral keys to be used for this handshake
    let x = curve25519::StaticSecret::random_from_rng(rng.rng_compat());
    client_send_intro_no_keygen(x, service, intro_header, plaintext_body)
}

/// Helper: Behaves like client_send_intro, but takes an ephemeral key x rather
/// than an RNG.
fn client_send_intro_no_keygen(
    x: curve25519::StaticSecret,
    service: &HsNtorServiceInfo,
    intro_header: &[u8],
    plaintext_body: &[u8],
) -> Result<(HsNtorClientState, Vec<u8>)> {
    let X = curve25519::PublicKey::from(&x);

    // Get EXP(B,x)
    let bx = x.diffie_hellman(&service.B);

    // Compile our state structure
    let state = HsNtorClientState {
        service_info: service.clone(),
        x,
        X,
    };

    // Compute keys required to finish this part of the handshake
    let (enc_key, mac_key) = get_introduce1_key_material(
        &bx,
        &service.auth_key,
        &X,
        &service.B,
        &service.subcredential,
    )?;

    let (ciphertext, mac_tag) = encrypt_and_mac(
        plaintext_body,
        intro_header,
        X.as_bytes(),
        &enc_key,
        mac_key,
    );

    // Create the relevant parts of INTRO1
    let mut response: Vec<u8> = Vec::new();
    response
        .write(&X)
        .and_then(|_| response.write(&ciphertext))
        .and_then(|_| response.write(&mac_tag))
        .map_err(into_internal!("Can't encode hs-ntor client handshake."))?;

    Ok((state, response))
}

/// The introduction has been completed and the service has replied with a
/// RENDEZVOUS1.
///
/// Handle it by computing and verifying the MAC, and if it's legit return a
/// key generator based on the result of the key exchange.
pub fn client_receive_rend<T>(state: &HsNtorClientState, msg: T) -> Result<HsNtorHkdfKeyGenerator>
where
    T: AsRef<[u8]>,
{
    // Extract the public key of the service from the message
    let mut cur = Reader::from_slice(msg.as_ref());
    let Y: curve25519::PublicKey = cur
        .extract()
        .map_err(|e| Error::from_bytes_err(e, "hs_ntor handshake"))?;
    let mac_tag: MacTag = cur
        .extract()
        .map_err(|e| Error::from_bytes_err(e, "hs_ntor handshake"))?;

    // Get EXP(Y,x) and EXP(B,x)
    let xy = state.x.diffie_hellman(&Y);
    let xb = state.x.diffie_hellman(&state.service_info.B);

    let (keygen, my_mac_tag) = get_rendezvous1_key_material(
        &xy,
        &xb,
        &state.service_info.auth_key,
        &state.service_info.B,
        &state.X,
        &Y,
    )?;

    // Validate the MAC!
    if my_mac_tag != mac_tag {
        return Err(Error::BadCircHandshakeAuth);
    }

    Ok(keygen)
}

/*********************** Server Side Code ************************************/

/// The input required to enter the HS Ntor protocol as a service
pub struct HsNtorServiceInput {
    /// Introduction point encryption privkey
    b: HsSvcNtorSecretKey,
    /// Introduction point encryption pubkey
    B: HsSvcNtorKey,

    /// Introduction point authentication key (aka AUTH_KEY)
    auth_key: HsIntroPtSessionIdKey,

    /// Our subcredential
    subcredential: Subcredential,
}

impl HsNtorServiceInput {
    /// Create a new `HsNtorServiceInput`
    pub fn new(
        b: HsSvcNtorSecretKey,
        B: HsSvcNtorKey,
        auth_key: HsIntroPtSessionIdKey,
        subcredential: Subcredential,
    ) -> Self {
        HsNtorServiceInput {
            b,
            B,
            auth_key,
            subcredential,
        }
    }
}

/// Conduct the HS Ntor handshake as the service.
///
/// Return a key generator which is the result of the key exchange, the
/// RENDEZVOUS1 response to the client, and the introduction plaintext that we decrypted.
///
/// The response to the client is:
///    SERVER_PK   Y                         [PK_PUBKEY_LEN bytes]
///    AUTH        AUTH_INPUT_MAC            [MAC_LEN bytes]
pub fn server_receive_intro<R>(
    rng: &mut R,
    proto_input: &HsNtorServiceInput,
    intro_header: &[u8],
    msg: &[u8],
) -> Result<(HsNtorHkdfKeyGenerator, Vec<u8>, Vec<u8>)>
where
    R: RngCore + CryptoRng,
{
    let y = curve25519::StaticSecret::random_from_rng(rng.rng_compat());
    server_receive_intro_no_keygen(&y, proto_input, intro_header, msg)
}

/// Helper: Like server_receive_intro, but take an ephemeral key rather than a RNG.
fn server_receive_intro_no_keygen(
    // This should be an EphemeralSecret, but using a StaticSecret is necessary
    // so that we can make one from raw bytes in our test.
    y: &curve25519::StaticSecret,
    proto_input: &HsNtorServiceInput,
    intro_header: &[u8],
    msg: &[u8],
) -> Result<(HsNtorHkdfKeyGenerator, Vec<u8>, Vec<u8>)> {
    // Extract all the useful pieces from the message
    let mut cur = Reader::from_slice(msg);
    let X: curve25519::PublicKey = cur
        .extract()
        .map_err(|e| Error::from_bytes_err(e, "hs ntor handshake"))?;
    let remaining_bytes = cur.remaining();
    let ciphertext = &mut cur
        .take(remaining_bytes - 32)
        .map_err(|e| Error::from_bytes_err(e, "hs ntor handshake"))?
        .to_vec();
    let mac_tag: MacTag = cur
        .extract()
        .map_err(|e| Error::from_bytes_err(e, "hs ntor handshake"))?;

    // Now derive keys needed for handling the INTRO1 cell
    let bx = proto_input.b.as_ref().diffie_hellman(&X);
    let (enc_key, mac_key) = get_introduce1_key_material(
        &bx,
        &proto_input.auth_key,
        &X,
        &proto_input.B,
        &proto_input.subcredential,
    )?;

    // Now validate the MAC: Staple the previous INTRODUCE1 data along with the
    // ciphertext to create the body of the MAC tag
    let mut mac_body: Vec<u8> = Vec::new();
    mac_body.extend(intro_header);
    mac_body.extend(X.as_bytes());
    mac_body.extend(&ciphertext[..]);
    let my_mac_tag = hs_mac(&mac_key, &mac_body);

    if my_mac_tag != mac_tag {
        return Err(Error::BadCircHandshakeAuth);
    }

    // Decrypt the ENCRYPTED_DATA from the intro cell
    let zero_iv = GenericArray::default();
    let mut cipher = Aes256Ctr::new(enc_key.as_ref().into(), &zero_iv);
    cipher.apply_keystream(ciphertext);
    let plaintext = ciphertext; // it's now decrypted

    // Generate ephemeral keys for this handshake
    let Y = curve25519::PublicKey::from(y);

    // Compute EXP(X,y) and EXP(X,b)
    let xy = y.diffie_hellman(&X);
    let xb = proto_input.b.as_ref().diffie_hellman(&X);

    let (keygen, auth_input_mac) =
        get_rendezvous1_key_material(&xy, &xb, &proto_input.auth_key, &proto_input.B, &X, &Y)?;

    // Set up RENDEZVOUS1 reply to the client
    let mut reply: Vec<u8> = Vec::new();
    reply
        .write(&Y)
        .and_then(|_| reply.write(&auth_input_mac))
        .map_err(into_internal!("Can't encode hs-ntor server handshake."))?;

    Ok((keygen, reply, plaintext.clone()))
}

/*********************** Helper functions ************************************/

/// Helper function: Compute the part of the HS ntor handshake that generates
/// key material for creating and handling INTRODUCE1 cells. Function used
/// by both client and service. Specifically, calculate the following:
///
/// ```pseudocode
///  intro_secret_hs_input = EXP(B,x) | AUTH_KEY | X | B | PROTOID
///  info = m_hsexpand | subcredential
///  hs_keys = KDF(intro_secret_hs_input | t_hsenc | info, S_KEY_LEN+MAC_LEN)
///  ENC_KEY = hs_keys[0:S_KEY_LEN]
///  MAC_KEY = hs_keys[S_KEY_LEN:S_KEY_LEN+MAC_KEY_LEN]
/// ```
///
/// Return (ENC_KEY, MAC_KEY).
fn get_introduce1_key_material(
    bx: &curve25519::SharedSecret,
    auth_key: &ed25519::PublicKey,
    X: &curve25519::PublicKey,
    B: &curve25519::PublicKey,
    subcredential: &Subcredential,
) -> Result<(EncKey, MacKey)> {
    let hs_ntor_protoid_constant = &b"tor-hs-ntor-curve25519-sha3-256-1"[..];
    let hs_ntor_key_constant = &b"tor-hs-ntor-curve25519-sha3-256-1:hs_key_extract"[..];
    let hs_ntor_expand_constant = &b"tor-hs-ntor-curve25519-sha3-256-1:hs_key_expand"[..];

    // Construct hs_keys = KDF(intro_secret_hs_input | t_hsenc | info, S_KEY_LEN+MAC_LEN)
    // Start by getting 'intro_secret_hs_input'
    let mut secret_input = SecretBuf::new();
    secret_input
        .write(bx) // EXP(B,x)
        .and_then(|_| secret_input.write(auth_key)) // AUTH_KEY
        .and_then(|_| secret_input.write(X)) // X
        .and_then(|_| secret_input.write(B)) // B
        .and_then(|_| secret_input.write(hs_ntor_protoid_constant)) // PROTOID
        // Now fold in the t_hsenc
        .and_then(|_| secret_input.write(hs_ntor_key_constant))
        // and fold in the 'info'
        .and_then(|_| secret_input.write(hs_ntor_expand_constant))
        .and_then(|_| secret_input.write(subcredential))
        .map_err(into_internal!("Can't generate hs-ntor kdf input."))?;

    let hs_keys = ShakeKdf::new().derive(&secret_input[..], 32 + 32)?;
    // Extract the keys into arrays
    let enc_key = Zeroizing::new(
        hs_keys[0..32]
            .try_into()
            .map_err(into_internal!("converting enc_key"))
            .map_err(Error::from)?,
    );
    let mac_key = hs_keys[32..64]
        .try_into()
        .map_err(into_internal!("converting mac_key"))
        .map_err(Error::from)?;

    Ok((enc_key, mac_key))
}

/// Helper function: Compute the last part of the HS ntor handshake which
/// derives key material necessary to create and handle RENDEZVOUS1
/// cells. Function used by both client and service. The actual calculations is
/// as follows:
///
///  rend_secret_hs_input = EXP(X,y) | EXP(X,b) | AUTH_KEY | B | X | Y | PROTOID
///  NTOR_KEY_SEED = MAC(rend_secret_hs_input, t_hsenc)
///  verify = MAC(rend_secret_hs_input, t_hsverify)
///  auth_input = verify | AUTH_KEY | B | Y | X | PROTOID | "Server"
///  AUTH_INPUT_MAC = MAC(auth_input, t_hsmac)
///
/// Return (keygen, AUTH_INPUT_MAC), where keygen is a key generator based on
/// NTOR_KEY_SEED.
fn get_rendezvous1_key_material(
    xy: &curve25519::SharedSecret,
    xb: &curve25519::SharedSecret,
    auth_key: &ed25519::PublicKey,
    B: &curve25519::PublicKey,
    X: &curve25519::PublicKey,
    Y: &curve25519::PublicKey,
) -> Result<(HsNtorHkdfKeyGenerator, AuthInputMac)> {
    let hs_ntor_protoid_constant = &b"tor-hs-ntor-curve25519-sha3-256-1"[..];
    let hs_ntor_mac_constant = &b"tor-hs-ntor-curve25519-sha3-256-1:hs_mac"[..];
    let hs_ntor_verify_constant = &b"tor-hs-ntor-curve25519-sha3-256-1:hs_verify"[..];
    let server_string_constant = &b"Server"[..];
    let hs_ntor_expand_constant = &b"tor-hs-ntor-curve25519-sha3-256-1:hs_key_expand"[..];
    let hs_ntor_key_constant = &b"tor-hs-ntor-curve25519-sha3-256-1:hs_key_extract"[..];

    // Start with rend_secret_hs_input
    let mut secret_input = SecretBuf::new();
    secret_input
        .write(xy) // EXP(X,y)
        .and_then(|_| secret_input.write(xb)) // EXP(X,b)
        .and_then(|_| secret_input.write(auth_key)) // AUTH_KEY
        .and_then(|_| secret_input.write(B)) // B
        .and_then(|_| secret_input.write(X)) // X
        .and_then(|_| secret_input.write(Y)) // Y
        .and_then(|_| secret_input.write(hs_ntor_protoid_constant)) // PROTOID
        .map_err(into_internal!(
            "Can't encode input to hs-ntor key derivation."
        ))?;

    // Build NTOR_KEY_SEED and verify
    let ntor_key_seed = hs_mac(&secret_input, hs_ntor_key_constant);
    let verify = hs_mac(&secret_input, hs_ntor_verify_constant);

    // Start building 'auth_input'
    let mut auth_input = Vec::new();
    auth_input
        .write(&verify)
        .and_then(|_| auth_input.write(auth_key)) // AUTH_KEY
        .and_then(|_| auth_input.write(B)) // B
        .and_then(|_| auth_input.write(Y)) // Y
        .and_then(|_| auth_input.write(X)) // X
        .and_then(|_| auth_input.write(hs_ntor_protoid_constant)) // PROTOID
        .and_then(|_| auth_input.write(server_string_constant)) // "Server"
        .map_err(into_internal!("Can't encode auth-input for hs-ntor."))?;

    // Get AUTH_INPUT_MAC
    let auth_input_mac = hs_mac(&auth_input, hs_ntor_mac_constant);

    // Now finish up with the KDF construction
    let mut kdf_seed = SecretBuf::new();
    kdf_seed
        .write(&ntor_key_seed)
        .and_then(|_| kdf_seed.write(hs_ntor_expand_constant))
        .map_err(into_internal!("Can't encode kdf-input for hs-ntor."))?;
    let keygen = HsNtorHkdfKeyGenerator::new(kdf_seed);

    Ok((keygen, auth_input_mac))
}

/*********************** Unit Tests ******************************************/

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->
    use super::*;
    use hex_literal::hex;
    use tor_basic_utils::test_rng::testing_rng;

    #[test]
    /// Basic HS Ntor test that does the handshake between client and service
    /// and makes sure that the resulting keys and KDF is legit.
    fn hs_ntor() -> Result<()> {
        let mut rng = testing_rng().rng_compat();

        // Let's initialize keys for the client (and the intro point)
        let intro_b_privkey = curve25519::StaticSecret::random_from_rng(&mut rng);
        let intro_b_pubkey = curve25519::PublicKey::from(&intro_b_privkey);
        let intro_auth_key_privkey = ed25519::SecretKey::generate(&mut rng);
        let intro_auth_key_pubkey = ed25519::PublicKey::from(&intro_auth_key_privkey);
        drop(intro_auth_key_privkey); // not actually used in this part of the protocol.

        // Create keys for client and service
        let client_keys = HsNtorServiceInfo::new(
            intro_b_pubkey.into(),
            intro_auth_key_pubkey.into(),
            [5; 32].into(),
        );

        let service_keys = HsNtorServiceInput::new(
            intro_b_privkey.into(),
            intro_b_pubkey.into(),
            intro_auth_key_pubkey.into(),
            [5; 32].into(),
        );

        // Client: Sends an encrypted INTRODUCE1 cell
        let (state, cmsg) = client_send_intro(&mut rng, &client_keys, &[66; 10], &[42; 60])?;

        // Service: Decrypt INTRODUCE1 cell, and reply with RENDEZVOUS1 cell
        let (skeygen, smsg, s_plaintext) =
            server_receive_intro(&mut rng, &service_keys, &[66; 10], &cmsg)?;

        // Check that the plaintext received by the service is the one that the
        // client sent
        assert_eq!(s_plaintext, vec![42; 60]);

        // Client: Receive RENDEZVOUS1 and create key material
        let ckeygen = client_receive_rend(&state, smsg)?;

        // Test that RENDEZVOUS1 key material match
        let skeys = skeygen.expand(128)?;
        let ckeys = ckeygen.expand(128)?;
        assert_eq!(skeys, ckeys);

        Ok(())
    }

    #[test]
    /// Test vectors generated with hs_ntor_ref.py from little-t-tor.
    fn ntor_mac() {
        let result = hs_mac("who".as_bytes(), b"knows?");
        assert_eq!(
            &result,
            &hex!("5e7da329630fdaa3eab7498bb1dc625bbb9ca968f10392b6af92d51d5db17473").into()
        );

        let result = hs_mac("gone".as_bytes(), b"by");
        assert_eq!(
            &result,
            &hex!("90071aabb06d3f7c777db41542f4790c7dd9e2e7b2b842f54c9c42bbdb37e9a0").into()
        );
    }
}
