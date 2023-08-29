//! Implementation for onion service descriptors.
//!
//! An onion service descriptor is a document generated by an onion service and
//! uploaded to one or more HsDir nodes for clients to later download.  It tells
//! the onion service client where to find the current introduction points for
//! the onion service, and how to connect to them.
//!
//! An onion service descriptor is more complicated than most other
//! documentation types, because it is partially encrypted.

mod desc_enc;

#[cfg(feature = "hs-service")]
mod build;
mod inner;
mod middle;
mod outer;

pub use desc_enc::DecryptionError;
use tor_basic_utils::rangebounds::RangeBoundsExt;
use tor_error::internal;

use crate::{NetdocErrorKind as EK, Result};

use tor_checkable::signed::{self, SignatureGated};
use tor_checkable::timed::{self, TimerangeBound};
use tor_checkable::{SelfSigned, Timebound};
use tor_hscrypto::pk::{
    HsBlindId, HsClientDescEncKey, HsClientDescEncSecretKey, HsIntroPtSessionIdKey, HsSvcNtorKey,
};
use tor_hscrypto::{RevisionCounter, Subcredential};
use tor_linkspec::EncodedLinkSpec;
use tor_llcrypto::pk::curve25519;
use tor_units::IntegerMinutes;

use derive_builder::Builder;
use smallvec::SmallVec;

use std::result::Result as StdResult;
use std::time::SystemTime;

#[cfg(feature = "hsdesc-inner-docs")]
#[cfg_attr(docsrs, doc(cfg(feature = "hsdesc-inner-docs")))]
pub use {inner::HsDescInner, middle::HsDescMiddle, outer::HsDescOuter};

#[cfg(feature = "hs-service")]
#[cfg_attr(docsrs, doc(cfg(feature = "hs-service")))]
pub use build::HsDescBuilder;

/// Metadata about an onion service descriptor, as stored at an HsDir.
///
/// This object is parsed from the outermost document of an onion service
/// descriptor, and used on the HsDir to maintain its index.  It does not
/// include the inner documents' information about introduction points, since the
/// HsDir cannot decrypt those without knowing the onion service's un-blinded
/// identity.
///
/// The HsDir caches this value, along with the original text of the descriptor.
#[cfg(feature = "hs-dir")]
#[allow(dead_code)] // TODO RELAY: Remove this.
pub struct StoredHsDescMeta {
    /// The blinded onion identity for this descriptor.  (This is the only
    /// identity that the HsDir knows.)
    blinded_id: HsBlindId,

    /// Information about the expiration and revision counter for this
    /// descriptor.
    idx_info: IndexInfo,
}

/// An unchecked StoredHsDescMeta: parsed, but not checked for liveness or validity.
#[cfg(feature = "hs-dir")]
pub type UncheckedStoredHsDescMeta =
    signed::SignatureGated<timed::TimerangeBound<StoredHsDescMeta>>;

/// Information about how long to hold a given onion service descriptor, and
/// when to replace it.
#[derive(Debug, Clone)]
#[allow(dead_code)] // TODO RELAY: Remove this if there turns out to be no need for it.
struct IndexInfo {
    /// The lifetime in minutes that this descriptor should be held after it is
    /// received.
    lifetime: IntegerMinutes<u16>,
    /// The expiration time on the `descriptor-signing-key-cert` included in this
    /// descriptor.
    signing_cert_expires: SystemTime,
    /// The revision counter on this descriptor: higher values should replace
    /// older ones.
    revision: RevisionCounter,
}

/// A decrypted, decoded onion service descriptor.
///
/// This object includes information from both the outer (plaintext) document of
/// the descriptor, and the inner (encrypted) documents.  It tells the client the
/// information it needs to contact the onion service, including necessary
/// introduction points and public keys.
#[derive(Debug, Clone)]
pub struct HsDesc {
    /// Information about the expiration and revision counter for this
    /// descriptor.
    #[allow(dead_code)] // TODO RELAY: Remove this if there turns out to be no need for it.
    idx_info: IndexInfo,

    /// The list of authentication types that this onion service supports.
    auth_required: Option<SmallVec<[IntroAuthType; 2]>>,

    /// If true, this a "single onion service" and is not trying to keep its own location private.
    is_single_onion_service: bool,

    /// One or more introduction points used to contact the onion service.
    intro_points: Vec<IntroPointDesc>,
    // /// A list of recognized CREATE handshakes that this onion service supports.
    //
    // TODO:  When someday we add a "create2 format" other than "hs-ntor", we
    // should turn this into a caret enum, record this info, and expose it.
    // create2_formats: Vec<u32>,
}

/// A type of authentication that is required when introducing to an onion
/// service.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Eq, PartialEq, derive_more::Display)]
pub enum IntroAuthType {
    /// Ed25519 authentication is required.
    #[display(fmt = "ed25519")]
    Ed25519,
}

/// Information in an onion service descriptor about a single
/// introduction point.
#[derive(Debug, Clone, amplify::Getters)]
#[derive(Builder)]
#[builder(pattern = "owned")] // mirrors HsDescBuilder
pub struct IntroPointDesc {
    /// The list of link specifiers needed to extend a circuit to the introduction point.
    ///
    /// These can include public keys and network addresses.
    ///
    /// Note that we do not enforce the presence of any link specifiers here;
    /// this means that you can't assume that an `IntroPointDesc` is a meaningful
    /// `ChanTarget` without some processing.
    //
    // The builder setter takes a `Vec` directly.  This seems fine.
    #[getter(skip)]
    link_specifiers: Vec<EncodedLinkSpec>,

    /// The key to be used to extend a circuit _to the introduction point_, using the
    /// ntor or ntor3 handshakes.  (`KP_ntor`)
    #[builder(setter(name = "ipt_kp_ntor"))] // TODO rename the internal variable too
    ipt_ntor_key: curve25519::PublicKey,

    /// The key to be used to identify the onion service at this introduction point.
    /// (`KP_hs_ipt_sid`)
    #[builder(setter(name = "kp_hs_ipt_sid"))] // TODO rename the internal variable too
    ipt_sid_key: HsIntroPtSessionIdKey,

    /// `KP_hss_ntor`, the key used to encrypt a handshake _to the onion
    /// service_ when using this introduction point.
    ///
    /// The onion service uses a separate key of this type with each
    /// introduction point as part of its strategy for preventing replay
    /// attacks.
    #[builder(setter(name = "kp_hss_ntor"))] // TODO rename the internal variable too
    svc_ntor_key: HsSvcNtorKey,
}

/// An onion service after it has been parsed by the client, but not yet decrypted.
pub struct EncryptedHsDesc {
    /// The un-decoded outer document of our onion service descriptor.
    outer_doc: outer::HsDescOuter,
}

/// An unchecked HsDesc: parsed, but not checked for liveness or validity.
pub type UncheckedEncryptedHsDesc = signed::SignatureGated<timed::TimerangeBound<EncryptedHsDesc>>;

#[cfg(feature = "hs-dir")]
impl StoredHsDescMeta {
    // TODO relay: needs accessor functions too.  (Let's not use public fields; we
    // are likely to want to mess with the repr of these types.)

    /// Parse the outermost layer of the descriptor in `input`, and return the
    /// resulting metadata (if possible).
    pub fn parse(input: &str) -> Result<UncheckedStoredHsDescMeta> {
        let outer = outer::HsDescOuter::parse(input)?;
        Ok(outer.dangerously_map(|timebound| {
            timebound.dangerously_map(|outer| StoredHsDescMeta::from_outer_doc(&outer))
        }))
    }
}

impl HsDesc {
    /// Parse the outermost document of the descriptor in `input`, and validate
    /// that its identity is consistent with `blinded_onion_id`.
    ///
    /// On success, the caller will get a wrapped object which they must
    /// validate and then decrypt.
    ///
    /// Use [`HsDesc::parse_decrypt_validate`] if you just need an [`HsDesc`] and don't want to
    /// handle the validation/decryption of the wrapped object yourself.
    ///
    /// # Example
    /// ```
    /// # use hex_literal::hex;
    /// # use tor_checkable::{SelfSigned, Timebound};
    /// # use tor_netdoc::doc::hsdesc::HsDesc;
    /// # use tor_netdoc::Error;
    /// #
    /// # let unparsed_desc: &str = include_str!("../../testdata/hsdesc1.txt");
    /// # let blinded_id =
    /// #    hex!("43cc0d62fc6252f578705ca645a46109e265290343b1137e90189744b20b3f2d").into();
    /// # let subcredential =
    /// #    hex!("78210A0D2C72BB7A0CAF606BCD938B9A3696894FDDDBC3B87D424753A7E3DF37").into();
    /// # let timestamp = humantime::parse_rfc3339("2023-01-23T15:00:00Z").unwrap();
    /// #
    /// // Parse the descriptor
    /// let unchecked_desc = HsDesc::parse(unparsed_desc, &blinded_id)?;
    /// // Validate the signature and timeliness of the outer document
    /// let checked_desc = unchecked_desc
    ///     .check_signature()?
    ///     .check_valid_at(&timestamp)?;
    /// // Decrypt the outer and inner layers of the descriptor
    /// let unchecked_decrypted_desc = checked_desc.decrypt(&subcredential, None)?;
    /// // Validate the signature and timeliness of the inner document
    /// let hsdesc = unchecked_decrypted_desc
    ///     .check_valid_at(&timestamp)?
    ///     .check_signature()?;
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn parse(
        input: &str,
        // We don't actually need this to parse the HsDesc, but we _do_ need it to prevent
        // a nasty pattern where we forget to check that we got the right one.
        blinded_onion_id: &HsBlindId,
    ) -> Result<UncheckedEncryptedHsDesc> {
        let outer = outer::HsDescOuter::parse(input)?;
        let mut id_matches = false;
        let result = outer.dangerously_map(|timebound| {
            timebound.dangerously_map(|outer| {
                id_matches = blinded_onion_id == &outer.blinded_id();
                EncryptedHsDesc::from_outer_doc(outer)
            })
        });
        if !id_matches {
            return Err(
                EK::BadObjectVal.with_msg("onion service descriptor did not have the expected ID")
            );
        }

        Ok(result)
    }

    /// A convenience function for parsing, decrypting and validating HS descriptors.
    ///
    /// This function:
    ///   * parses the outermost document of the descriptor in `input`, and validates that its
    ///   identity is consistent with `blinded_onion_id`.
    ///   * decrypts both layers of encryption in the onion service descriptor. If `hsc_desc_enc`
    ///   is provided, we use it to decrypt the inner encryption layer; otherwise, we require that
    ///   the inner document is encrypted using the "no client authorization" method.
    ///   * checks if both layers are valid at the `valid_at` timestamp
    ///   * validates the signatures on both layers
    ///
    /// Returns an error if the descriptor cannot be parsed, or if one of the validation steps
    /// fails.
    pub fn parse_decrypt_validate(
        input: &str,
        blinded_onion_id: &HsBlindId,
        valid_at: SystemTime,
        subcredential: &Subcredential,
        hsc_desc_enc: Option<(&HsClientDescEncKey, &HsClientDescEncSecretKey)>,
    ) -> StdResult<TimerangeBound<Self>, HsDescError> {
        use HsDescError as E;
        let unchecked_desc = Self::parse(input, blinded_onion_id)
            .map_err(E::OuterParsing)?
            .check_signature()
            .map_err(|e| E::OuterValidation(e.into()))?;

        let (inner_desc, new_bounds) = {
            // We use is_valid_at and dangerously_into_parts instead of check_valid_at because we
            // need the time bounds of the outer layer (for computing the intersection with the
            // time bounds of the inner layer).
            unchecked_desc
                .is_valid_at(&valid_at)
                .map_err(|e| E::OuterValidation(e.into()))?;
            // It's safe to use dangerously_into_parts() as we've just checked if unchecked_desc is
            // valid at the current time
            let (unchecked_desc, bounds) = unchecked_desc.dangerously_into_parts();
            let inner_timerangebound = unchecked_desc.decrypt(subcredential, hsc_desc_enc)?;

            let new_bounds = bounds
                .intersect(&inner_timerangebound)
                .map(|(b1, b2)| (b1.cloned(), b2.cloned()));

            (inner_timerangebound, new_bounds)
        };

        let hsdesc = inner_desc
            .check_valid_at(&valid_at)
            .map_err(|e| E::InnerValidation(e.into()))?
            .check_signature()
            .map_err(|e| E::InnerValidation(e.into()))?;

        // If we've reached this point, it means the descriptor is valid at specified time. This
        // means the time bounds of the two layers definitely intersect, so new_bounds **must** be
        // Some. It is a bug if new_bounds is None.
        let new_bounds = new_bounds
            .ok_or_else(|| internal!("failed to compute TimerangeBounds for a valid descriptor"))?;

        Ok(TimerangeBound::new(hsdesc, new_bounds))
    }

    /// One or more introduction points used to contact the onion service.
    ///
    /// Always returns at least one introduction point,
    /// and never more than [`NUM_INTRO_POINT_MAX`](tor_hscrypto::NUM_INTRO_POINT_MAX).
    /// (Descriptors which have fewer or more are dealt with during parsing.)
    ///
    /// Accessor function.
    //
    // TODO: We'd like to derive this, but amplify::Getters  would give us &Vec<>,
    // not &[].
    //
    // Perhaps someday we can use derive_adhoc, or add as_ref() support?
    pub fn intro_points(&self) -> &[IntroPointDesc] {
        &self.intro_points
    }

    /// Return true if this onion service claims to be a non-anonymous "single
    /// onion service".
    ///
    /// (We should always anonymize our own connection to an onion service.)
    pub fn is_single_onion_service(&self) -> bool {
        self.is_single_onion_service
    }

    /// Return true if this onion service claims that it needs user authentication
    /// of some kind in its INTRODUCE messages.
    ///
    /// (Arti does not currently support sending this kind of authentication.)
    pub fn requires_intro_authentication(&self) -> bool {
        self.auth_required.is_some()
    }
}

/// An error returned by [`HsDesc::parse_decrypt_validate`], indicating what
/// kind of failure prevented us from validating an onion service descriptor.
///
/// This is distinct from [`tor_netdoc::Error`](crate::Error) so that we can
/// tell errors that could be the HsDir's fault from those that are definitely
/// protocol violations by the onion service.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HsDescError {
    /// An outer object failed parsing: the HsDir should probably have
    /// caught this, and not given us this HsDesc.
    ///
    /// (This can be an innocent error if we happen to know about restrictions
    /// that the HsDir does not).
    #[error("Parsing failure on outer layer of an onion service descriptor.")]
    OuterParsing(#[source] crate::Error),

    /// An outer object failed validation: the HsDir should probably have
    /// caught this, and not given us this HsDesc.
    ///
    /// (This can happen erroneously if we think that something is untimely but
    /// the HSDir's clock is slightly different, or _was_ different when it
    /// decided to give us this object.)
    #[error("Validation failure on outer layer of an onion service descriptor.")]
    OuterValidation(#[source] crate::Error),

    /// Decrypting the inner layer failed because we need to have a decryption key,
    /// but we didn't provide one.
    ///
    /// This is probably our fault.
    #[error("Decryption failure on onion service descriptor: missing decryption key")]
    MissingDecryptionKey,

    /// Decrypting the inner layer failed because, although we provided a key,
    /// we did not provide the key we need to decrypt it.
    ///
    /// This is probably our fault.
    #[error("Decryption failure on onion service descriptor: incorrect decryption key")]
    WrongDecryptionKey,

    /// Decrypting the inner or middle layer failed because of an issue with the
    /// decryption itself.
    ///
    /// This is the onion service's fault.
    #[error("Decryption failure on onion service descriptor: could not decrypt")]
    DecryptionFailed,

    /// We failed to parse something cryptographic in an inner layer of the
    /// onion service descriptor.
    ///
    /// This is definitely the onion service's fault.
    #[error("Parsing failure on inner layer of an onion service descriptor")]
    InnerParsing(#[source] crate::Error),

    /// We failed to validate something cryptographic in an inner layer of the
    /// onion service descriptor.
    ///
    /// This is definitely the onion service's fault.
    #[error("Validation failure on inner layer of an onion service descriptor")]
    InnerValidation(#[source] crate::Error),

    /// We encountered an internal error.
    #[error("Internal error: {0}")]
    Bug(#[from] tor_error::Bug),
}

impl tor_error::HasKind for HsDescError {
    fn kind(&self) -> tor_error::ErrorKind {
        use tor_error::ErrorKind as EK;
        use HsDescError as E;
        match self {
            E::OuterParsing(_) | E::OuterValidation(_) => EK::TorProtocolViolation,
            E::MissingDecryptionKey => EK::OnionServiceMissingClientAuth,
            E::WrongDecryptionKey => EK::OnionServiceWrongClientAuth,
            E::DecryptionFailed | E::InnerParsing(_) | E::InnerValidation(_) => {
                EK::OnionServiceProtocolViolation
            }
            E::Bug(e) => e.kind(),
        }
    }
}

impl IntroPointDesc {
    /// Start building a description of an intro point
    pub fn builder() -> IntroPointDescBuilder {
        IntroPointDescBuilder::default()
    }

    /// The list of link specifiers needed to extend a circuit to the introduction point.
    ///
    /// These can include public keys and network addresses.
    ///
    /// Accessor function.
    //
    // TODO: It would be better to derive this too, but this accessor needs to
    // return a slice; Getters can only give us a &Vec<> in this case.
    pub fn link_specifiers(&self) -> &[EncodedLinkSpec] {
        &self.link_specifiers
    }
}

impl EncryptedHsDesc {
    /// Attempt to decrypt both layers of encryption in this onion service
    /// descriptor.
    ///
    /// If `hsc_desc_enc` is provided, we use it to decrypt the inner encryption layer;
    /// otherwise, we require that the inner document is encrypted using the "no
    /// client authorization" method.
    ///
    /// Note that `hsc_desc_enc` must be a key *pair* - ie, a KP_hsc_desc_enc
    /// and corresponding KS_hsc_desc_enc. This function **does not check**
    /// this.
    //
    // TODO: Someday we _might_ want to allow a list of keypairs in place of
    // `hs_desc_enc`.  For now, though, we always know a single key that we want
    // to try using, and we don't want to leak any extra information by
    // providing other keys that _might_ work.  We certainly don't want to
    // encourage people to provide every key they know.
    pub fn decrypt(
        &self,
        subcredential: &Subcredential,
        hsc_desc_enc: Option<(&HsClientDescEncKey, &HsClientDescEncSecretKey)>,
    ) -> StdResult<TimerangeBound<SignatureGated<HsDesc>>, HsDescError> {
        use HsDescError as E;
        let blinded_id = self.outer_doc.blinded_id();
        let revision_counter = self.outer_doc.revision_counter();
        let kp_desc_sign = self.outer_doc.desc_sign_key_id();

        // Decrypt the superencryption layer; parse the middle document.
        let middle = self
            .outer_doc
            .decrypt_body(subcredential)
            .map_err(|_| E::DecryptionFailed)?;
        let middle = std::str::from_utf8(&middle[..]).map_err(|_| {
            E::InnerParsing(EK::BadObjectVal.with_msg("Bad utf-8 in middle document"))
        })?;
        let middle = middle::HsDescMiddle::parse(middle).map_err(E::InnerParsing)?;

        // Decrypt the encryption layer and parse the inner document.
        let inner = middle.decrypt_inner(
            &blinded_id,
            revision_counter,
            subcredential,
            hsc_desc_enc.map(|keys| keys.1),
        )?;
        let inner = std::str::from_utf8(&inner[..]).map_err(|_| {
            E::InnerParsing(EK::BadObjectVal.with_msg("Bad utf-8 in inner document"))
        })?;
        let (cert_signing_key, time_bound) =
            inner::HsDescInner::parse(inner).map_err(E::InnerParsing)?;

        if cert_signing_key.as_ref() != Some(kp_desc_sign) {
            return Err(E::InnerValidation(EK::BadObjectVal.with_msg(
                "Signing keys in inner document did not match those in outer document",
            )));
        }

        // Construct the HsDesc!
        let time_bound = time_bound.dangerously_map(|sig_bound| {
            sig_bound.dangerously_map(|inner| HsDesc {
                idx_info: IndexInfo::from_outer_doc(&self.outer_doc),
                auth_required: inner.intro_auth_types,
                is_single_onion_service: inner.single_onion_service,
                intro_points: inner.intro_points,
            })
        });
        Ok(time_bound)
    }

    /// Create a new `IndexInfo` from the outer part of an onion service descriptor.
    fn from_outer_doc(outer_layer: outer::HsDescOuter) -> Self {
        EncryptedHsDesc {
            outer_doc: outer_layer,
        }
    }
}

impl IndexInfo {
    /// Create a new `IndexInfo` from the outer part of an onion service descriptor.
    fn from_outer_doc(outer: &outer::HsDescOuter) -> Self {
        IndexInfo {
            lifetime: outer.lifetime,
            signing_cert_expires: outer.desc_signing_key_cert.expiry(),
            revision: outer.revision_counter(),
        }
    }
}

#[cfg(feature = "hs-dir")]
impl StoredHsDescMeta {
    /// Create a new `StoredHsDescMeta` from the outer part of an onion service descriptor.
    fn from_outer_doc(outer: &outer::HsDescOuter) -> Self {
        let blinded_id = outer.blinded_id();
        let idx_info = IndexInfo::from_outer_doc(outer);
        StoredHsDescMeta {
            blinded_id,
            idx_info,
        }
    }
}

/// Test data
#[cfg(any(test, feature = "testing"))]
#[allow(missing_docs)]
#[allow(clippy::missing_docs_in_private_items)]
pub mod test_data {
    use hex_literal::hex;

    pub const TEST_DATA: &str = include_str!("../../testdata/hsdesc1.txt");

    pub const TEST_SUBCREDENTIAL: [u8; 32] =
        hex!("78210A0D2C72BB7A0CAF606BCD938B9A3696894FDDDBC3B87D424753A7E3DF37");

    // This HsDesc uses DescEnc authentication.
    pub const TEST_DATA_2: &str = include_str!("../../testdata/hsdesc2.txt");
    pub const TEST_DATA_TIMEPERIOD_2: u64 = 19397;
    // paozpdhgz2okvc6kgbxvh2bnfsmt4xergrtcl4obkhopyvwxkpjzvoad.onion
    pub const TEST_HSID_2: [u8; 32] =
        hex!("781D978CE6CE9CAA8BCA306F53E82D2C993E5C91346625F1C151DCFC56D753D3");
    pub const TEST_SUBCREDENTIAL_2: [u8; 32] =
        hex!("24A133E905102BDA9A6AFE57F901366A1B8281865A91F1FE0853E4B50CC8B070");
    // SACGOAEODFGCYY22NYZV45ZESFPFLDGLMBWFACKEO34XGHASSAMQ (base32)
    pub const TEST_PUBKEY_2: [u8; 32] =
        hex!("900467008E194C2C635A6E335E7724915E558CCB606C50094476F9731C129019");
    // SDZNMD4RP4SCH4EYTTUZPFRZINNFWAOPPKZ6BINZAC7LREV24RBQ (base32)
    pub const TEST_SECKEY_2: [u8; 32] =
        hex!("90F2D60F917F2423F0989CE9979639435A5B01CF7AB3E0A1B900BEB892BAE443");
}

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
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->
    use std::time::Duration;

    use super::test_data::*;
    use super::*;
    use hex_literal::hex;
    use tor_hscrypto::{pk::HsIdKey, time::TimePeriod};
    use tor_llcrypto::pk::ed25519;

    #[test]
    #[cfg(feature = "hs-dir")]
    fn parse_meta_good() -> Result<()> {
        let meta = StoredHsDescMeta::parse(TEST_DATA)?
            .check_signature()?
            .check_valid_at(&humantime::parse_rfc3339("2023-01-23T15:00:00Z").unwrap())
            .unwrap();

        assert_eq!(
            meta.blinded_id.as_ref(),
            &hex!("43cc0d62fc6252f578705ca645a46109e265290343b1137e90189744b20b3f2d")
        );
        assert_eq!(
            Duration::try_from(meta.idx_info.lifetime).unwrap(),
            Duration::from_secs(60 * 180)
        );
        assert_eq!(
            meta.idx_info.signing_cert_expires,
            humantime::parse_rfc3339("2023-01-26T03:00:00Z").unwrap()
        );
        assert_eq!(meta.idx_info.revision, RevisionCounter::from(19655750));

        Ok(())
    }

    #[test]
    fn parse_desc_good() -> Result<()> {
        let wrong_blinded_id = [12; 32].into();
        let desc = HsDesc::parse(TEST_DATA, &wrong_blinded_id);
        assert!(desc.is_err());
        let blinded_id =
            hex!("43cc0d62fc6252f578705ca645a46109e265290343b1137e90189744b20b3f2d").into();
        let desc = HsDesc::parse(TEST_DATA, &blinded_id)?
            .check_signature()?
            .check_valid_at(&humantime::parse_rfc3339("2023-01-23T15:00:00Z").unwrap())
            .unwrap()
            .decrypt(&TEST_SUBCREDENTIAL.into(), None)
            .unwrap();
        let desc = desc
            .check_valid_at(&humantime::parse_rfc3339("2023-01-24T03:00:00Z").unwrap())
            .unwrap();
        let desc = desc.check_signature().unwrap();

        assert_eq!(
            Duration::try_from(desc.idx_info.lifetime).unwrap(),
            Duration::from_secs(60 * 180)
        );
        assert_eq!(
            desc.idx_info.signing_cert_expires,
            humantime::parse_rfc3339("2023-01-26T03:00:00Z").unwrap()
        );
        assert_eq!(desc.idx_info.revision, RevisionCounter::from(19655750));
        assert!(desc.auth_required.is_none());
        assert_eq!(desc.is_single_onion_service, false);
        assert_eq!(desc.intro_points.len(), 3);

        let ipt0 = &desc.intro_points()[0];
        assert_eq!(
            ipt0.ipt_ntor_key().as_bytes(),
            &hex!("553BF9F9E1979D6F5D5D7D20BB3FE7272E32E22B6E86E35C76A7CA8A377E402F")
        );
        // TODO TEST: Perhaps add tests for other intro point fields.

        Ok(())
    }

    /// Get an EncryptedHsDesc corresponding to `TEST_DATA_2`.
    fn get_test2_encrypted() -> EncryptedHsDesc {
        let id: HsIdKey = ed25519::PublicKey::from_bytes(&TEST_HSID_2).unwrap().into();
        let period = TimePeriod::new(
            humantime::parse_duration("24 hours").unwrap(),
            humantime::parse_rfc3339("2023-02-09T12:00:00Z").unwrap(),
            humantime::parse_duration("12 hours").unwrap(),
        )
        .unwrap();
        assert_eq!(period.interval_num(), TEST_DATA_TIMEPERIOD_2);
        let (blind_id, subcredential) = id.compute_blinded_key(period).unwrap();

        assert_eq!(
            blind_id.as_bytes(),
            &hex!("706628758208395D461AA0F460A5E76E7B828C66B5E794768592B451302E961D")
        );

        assert_eq!(subcredential.as_ref(), &TEST_SUBCREDENTIAL_2);

        HsDesc::parse(TEST_DATA_2, &blind_id.into())
            .unwrap()
            .check_signature()
            .unwrap()
            .check_valid_at(&humantime::parse_rfc3339("2023-02-09T12:00:00Z").unwrap())
            .unwrap()
    }

    #[test]
    fn parse_desc_auth_missing() {
        // If we try to decrypt TEST_DATA_2 with no ClientDescEncKey, we get a
        // failure.
        let encrypted = get_test2_encrypted();
        let subcredential = TEST_SUBCREDENTIAL_2.into();
        let with_no_auth = encrypted.decrypt(&subcredential, None);
        assert!(with_no_auth.is_err());
    }

    #[test]
    fn parse_desc_auth_good() {
        // But if we try to decrypt TEST_DATA_2 with the correct ClientDescEncKey, we get a
        // the data inside!

        let encrypted = get_test2_encrypted();
        let subcredential = TEST_SUBCREDENTIAL_2.into();
        let pk = curve25519::PublicKey::from(TEST_PUBKEY_2).into();
        let sk = curve25519::StaticSecret::from(TEST_SECKEY_2).into();
        let desc = encrypted.decrypt(&subcredential, Some((&pk, &sk))).unwrap();
        let desc = desc
            .check_valid_at(&humantime::parse_rfc3339("2023-01-24T03:00:00Z").unwrap())
            .unwrap();
        let desc = desc.check_signature().unwrap();
        assert_eq!(desc.intro_points.len(), 3);
    }
}
