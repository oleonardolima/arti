//! [`KeySpecifier`] implementations for hidden service keys.

use std::fmt;

use tor_hscrypto::time::TimePeriod;
use tor_keymgr::{ArtiPath, CTorPath, KeySpecifier};

use crate::HsNickname;

/// An identifier for a particular instance of a hidden service key.
pub struct HsSvcKeySpecifier {
    /// The nickname of the  hidden service.
    nickname: HsNickname,
    /// The role of this key
    role: HsSvcKeyRole,
}

impl HsSvcKeySpecifier {
    /// Create a new specifier for service the service with the specified `nickname`.
    pub fn new(nickname: HsNickname, role: HsSvcKeyRole) -> Self {
        Self { nickname, role }
    }
}

/// The role of a hidden service key
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum HsSvcKeyRole {
    /// The blinded signing key.`
    BlindIdSecretKey(TimePeriod),
}

impl fmt::Display for HsSvcKeyRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use HsSvcKeyRole::*;

        match self {
            BlindIdSecretKey(period) => write!(
                f,
                "KS_hs_blind_id_{}_{}",
                period.interval_num(),
                period.length()
            ),
        }
    }
}

impl KeySpecifier for HsSvcKeySpecifier {
    fn arti_path(&self) -> tor_keymgr::Result<ArtiPath> {
        ArtiPath::new(format!("service/{}/{}", self.nickname, self.role))
    }

    fn ctor_path(&self) -> Option<CTorPath> {
        todo!()
    }
}
