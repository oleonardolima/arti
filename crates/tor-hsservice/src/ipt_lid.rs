//! [`IptLocalId`]

use super::*;

/// Persistent local identifier for an introduction point
///
/// Changes when the IPT relay changes, or the IPT key material changes.
/// (Different for different `.onion` services, obviously)
///
/// Is a randomly-generated byte string, currently 32 long.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Adhoc)]
#[derive_adhoc(SerdeStringOrTransparent)]
#[cfg_attr(test, derive(derive_more::From))]
pub(crate) struct IptLocalId([u8; 32]);

impl_debug_hex!(IptLocalId.0);

impl Display for IptLocalId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for v in self.0 {
            write!(f, "{v:02x}")?;
        }
        Ok(())
    }
}

/// Invalid [`IptLocalId`] - for example bad string representation
#[derive(Debug, Error, Clone, Eq, PartialEq)]
#[error("invalid IptLocalId")]
#[non_exhaustive]
pub(crate) struct InvalidIptLocalId {}

impl FromStr for IptLocalId {
    type Err = InvalidIptLocalId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut b = [0; 32];
        hex::decode_to_slice(s, &mut b).map_err(|_: hex::FromHexError| InvalidIptLocalId {})?;
        Ok(IptLocalId(b))
    }
}

impl KeySpecifierComponentViaDisplayFromStr for IptLocalId {}

impl IptLocalId {
    /// Return a fixed dummy `IptLocalId`, for testing etc.
    ///
    /// The id is made by repeating `which` 32 times.
    #[cfg(test)]
    pub(crate) fn dummy(which: u8) -> Self {
        IptLocalId([which; 32]) // I can't think of a good way not to specify 32 again here
    }
}

impl rand::distributions::Distribution<IptLocalId> for rand::distributions::Standard {
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> IptLocalId {
        IptLocalId(rng.gen())
    }
}

#[cfg(test)]
pub(crate) mod test {
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
    use super::*;
    use itertools::{chain, Itertools};

    #[derive(Serialize, Deserialize, Eq, PartialEq, Debug)]
    struct IptLidTest {
        lid: IptLocalId,
    }

    #[test]
    fn lid_serde() {
        let t = IptLidTest {
            lid: IptLocalId::dummy(7),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(
            json,
            // This also tests <IptLocalId as Display> since that's how we serialise it
            r#"{"lid":"0707070707070707070707070707070707070707070707070707070707070707"}"#,
        );
        let u: IptLidTest = serde_json::from_str(&json).unwrap();
        assert_eq!(t, u);

        let mpack = rmp_serde::to_vec_named(&t).unwrap();
        assert_eq!(
            mpack,
            chain!(&[129, 163], b"lid", &[220, 0, 32], &[0x07; 32],)
                .cloned()
                .collect_vec()
        );
        let u: IptLidTest = rmp_serde::from_slice(&mpack).unwrap();
        assert_eq!(t, u);
    }
}
