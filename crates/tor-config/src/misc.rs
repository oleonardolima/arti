//! Miscellaneous types used in configuration
//!
//! This module contains types that need to be shared across various crates
//! and layers, but which don't depend on specific elements of the Tor system.

use std::borrow::Cow;
use std::fmt::{Debug, Display};
use std::iter;
use std::net;
use std::num::NonZeroU16;

use either::Either;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// Boolean, but with additional `"auto"` option
//
// This slightly-odd interleaving of derives and attributes stops rustfmt doing a daft thing
#[derive(Clone, Copy, Hash, Debug, Default, Ord, PartialOrd, Eq, PartialEq)]
#[allow(clippy::exhaustive_enums)] // we will add variants very rarely if ever
#[derive(Serialize, Deserialize)]
#[serde(try_from = "BoolOrAutoSerde", into = "BoolOrAutoSerde")]
pub enum BoolOrAuto {
    #[default]
    /// Automatic
    Auto,
    /// Explicitly specified
    Explicit(bool),
}

impl BoolOrAuto {
    /// Returns the explicitly set boolean value, or `None`
    ///
    /// ```
    /// use tor_config::BoolOrAuto;
    ///
    /// fn calculate_default() -> bool { //...
    /// # false }
    /// let bool_or_auto: BoolOrAuto = // ...
    /// # Default::default();
    /// let _: bool = bool_or_auto.as_bool().unwrap_or_else(|| calculate_default());
    /// ```
    pub fn as_bool(self) -> Option<bool> {
        match self {
            BoolOrAuto::Auto => None,
            BoolOrAuto::Explicit(v) => Some(v),
        }
    }
}

/// How we (de) serialize a [`BoolOrAuto`]
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum BoolOrAutoSerde {
    /// String (in snake case)
    String(Cow<'static, str>),
    /// bool
    Bool(bool),
}

impl From<BoolOrAuto> for BoolOrAutoSerde {
    fn from(boa: BoolOrAuto) -> BoolOrAutoSerde {
        use BoolOrAutoSerde as BoAS;
        boa.as_bool()
            .map(BoAS::Bool)
            .unwrap_or_else(|| BoAS::String("auto".into()))
    }
}

/// Boolean or `"auto"` configuration is invalid
#[derive(thiserror::Error, Debug, Clone)]
#[non_exhaustive]
#[error(r#"Invalid value, expected boolean or "auto""#)]
pub struct InvalidBoolOrAuto {}

impl TryFrom<BoolOrAutoSerde> for BoolOrAuto {
    type Error = InvalidBoolOrAuto;

    fn try_from(pls: BoolOrAutoSerde) -> Result<BoolOrAuto, Self::Error> {
        use BoolOrAuto as BoA;
        use BoolOrAutoSerde as BoAS;
        Ok(match pls {
            BoAS::Bool(v) => BoA::Explicit(v),
            BoAS::String(s) if s == "false" => BoA::Explicit(false),
            BoAS::String(s) if s == "true" => BoA::Explicit(true),
            BoAS::String(s) if s == "auto" => BoA::Auto,
            _ => return Err(InvalidBoolOrAuto {}),
        })
    }
}

/// A macro that implements [`NotAutoValue`] for your type.
///
/// This macro generates:
///   * a [`NotAutoValue`] impl for `ty`
///   * a test module with a test that ensures "auto" cannot be deserialized as `ty`
///
/// ## Example
///
/// ```rust
/// # use tor_config::{impl_not_auto_value, ExplicitOrAuto};
/// # use serde::{Serialize, Deserialize};
//  #
/// #[derive(Serialize, Deserialize)]
/// struct Foo;
///
/// impl_not_auto_value!(Foo);
///
/// #[derive(Serialize, Deserialize)]
/// struct Bar;
///
/// fn main() {
///    let _foo: ExplicitOrAuto<Foo> = ExplicitOrAuto::Auto;
///
///    // Using a type that does not implement NotAutoValue is an error:
///    // let _bar: ExplicitOrAuto<Bar> = ExplicitOrAuto::Auto;
/// }
/// ```
#[macro_export]
macro_rules! impl_not_auto_value {
    ($ty:ty) => {
        $crate::deps::paste! {
            impl $crate::NotAutoValue for $ty {}

            #[cfg(test)]
            #[allow(non_snake_case)]
            mod [<test_not_auto_value_ $ty>] {
                #[allow(unused_imports)]
                use super::*;

                #[test]
                fn [<auto_is_not_a_valid_value_for_ $ty>]() {
                    let res = $crate::deps::serde_value::Value::String(
                        "auto".into()
                    ).deserialize_into::<$ty>();

                    assert!(
                        res.is_err(),
                        concat!(
                            stringify!($ty), " is not a valid NotAutoValue type: ",
                            "NotAutoValue types should not be deserializable from \"auto\""
                        ),
                    );
                }
            }
        }
    };
}

/// A serializable value, or auto.
///
/// Used for implementing configuration options that can be explicitly initialized
/// with a placeholder for their "default" value using the
/// [`Auto`](ExplicitOrAuto::Auto) variant.
///
/// Unlike `#[serde(default)] field: T` or `#[serde(default)] field: Option<T>`,
/// fields of this type can be present in the serialized configuration
/// without being assigned a concrete value.
///
/// **Important**: the underlying type must implement [`NotAutoValue`].
/// This trait should be implemented using the [`impl_not_auto_value`],
/// and only for types that do not serialize to the same value as the
/// [`Auto`](ExplicitOrAuto::Auto) variant.
///
/// ## Example
///
/// In the following serialized TOML config
///
/// ```toml
///  foo = "auto"
/// ```
///
/// `foo` is set to [`Auto`](ExplicitOrAuto::Auto), which indicates the
/// implementation should use a default (but not necessarily [`Default::default`])
/// value for the `foo` option.
///
/// For example, f field `foo` defaults to `13` if feature `bar` is enabled,
/// and `9000` otherise, a configuration with `foo` set to `"auto"` will
/// behave in the "default" way regardless of which features are enabled.
///
/// ```rust,ignore
/// struct Foo(usize);
///
/// impl Default for Foo {
///     fn default() -> Foo {
///         if cfg!(feature = "bar") {
///             Foo(13)
///         } else {
///             Foo(9000)
///         }
///     }
/// }
///
/// impl Foo {
///     fn from_explicit_or_auto(foo: ExplicitOrAuto<Foo>) -> Self {
///         match foo {
///             // If Auto, choose a sensible default for foo
///             ExplicitOrAuto::Auto => Default::default(),
///             ExplicitOrAuto::Foo(foo) => foo,
///         }
///     }
/// }
///
/// struct Config {
///    foo: ExplicitOrAuto<Foo>,
/// }
/// ```
#[derive(Clone, Copy, Hash, Debug, Default, Ord, PartialOrd, Eq, PartialEq)]
#[allow(clippy::exhaustive_enums)] // we will add variants very rarely if ever
#[derive(Serialize, Deserialize)]
pub enum ExplicitOrAuto<T: NotAutoValue> {
    /// Automatic
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// Explicitly specified
    #[serde(untagged)]
    Explicit(T),
}

impl<T: NotAutoValue> ExplicitOrAuto<T> {
    /// Returns the explicitly set value, or `None`.
    ///
    /// ```
    /// use tor_config::ExplicitOrAuto;
    ///
    /// fn calculate_default() -> usize { //...
    /// # 2 }
    /// let explicit_or_auto: ExplicitOrAuto<usize> = // ...
    /// # Default::default();
    /// let _: usize = explicit_or_auto.into_value().unwrap_or_else(|| calculate_default());
    /// ```
    pub fn into_value(self) -> Option<T> {
        match self {
            ExplicitOrAuto::Auto => None,
            ExplicitOrAuto::Explicit(v) => Some(v),
        }
    }

    /// Returns a reference to the explicitly set value, or `None`.
    ///
    /// Like [`ExplicitOrAuto::into_value`], except it returns a reference to the inner type.
    pub fn as_value(&self) -> Option<&T> {
        match self {
            ExplicitOrAuto::Auto => None,
            ExplicitOrAuto::Explicit(v) => Some(v),
        }
    }
}

/// A marker trait for types that do not serialize to the same value as [`ExplicitOrAuto::Auto`].
///
/// **Important**: you should not implement this trait manually.
/// Use the [`impl_not_auto_value`] macro instead.
///
/// This trait should be implemented for types that can be stored in [`ExplicitOrAuto`].
pub trait NotAutoValue {}

/// A helper for calling [`impl_not_auto_value`] for a number of types.
macro_rules! impl_not_auto_value_for_types {
    ($($ty:ty)*) => {
        $(impl_not_auto_value!($ty);)*
    }
}

// Implement `NotAutoValue` for various primitive types.
impl_not_auto_value_for_types!(
    i8 i16 i32 i64 i128 isize
    u8 u16 u32 u64 u128 usize
    f32 f64
    char
    bool
);

// TODO implement `NotAutoValue` for other types too

/// Padding enablement - rough amount of padding requested
///
/// Padding is cover traffic, used to help mitigate traffic analysis,
/// obscure traffic patterns, and impede router-level data collection.
///
/// This same enum is used to control padding at various levels of the Tor system.
/// (TODO: actually we don't do circuit padding yet.)
//
// This slightly-odd interleaving of derives and attributes stops rustfmt doing a daft thing
#[derive(Clone, Copy, Hash, Debug, Ord, PartialOrd, Eq, PartialEq)]
#[allow(clippy::exhaustive_enums)] // we will add variants very rarely if ever
#[derive(Serialize, Deserialize)]
#[serde(try_from = "PaddingLevelSerde", into = "PaddingLevelSerde")]
#[derive(Display, EnumString, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[derive(Default)]
pub enum PaddingLevel {
    /// Disable padding completely
    None,
    /// Reduced padding (eg for mobile)
    Reduced,
    /// Normal padding (the default)
    #[default]
    Normal,
}

/// How we (de) serialize a [`PaddingLevel`]
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PaddingLevelSerde {
    /// String (in snake case)
    ///
    /// We always serialize this way
    String(Cow<'static, str>),
    /// bool
    Bool(bool),
}

impl From<PaddingLevel> for PaddingLevelSerde {
    fn from(pl: PaddingLevel) -> PaddingLevelSerde {
        PaddingLevelSerde::String(<&str>::from(&pl).into())
    }
}

/// Padding level configuration is invalid
#[derive(thiserror::Error, Debug, Clone)]
#[non_exhaustive]
#[error("Invalid padding level")]
struct InvalidPaddingLevel {}

impl TryFrom<PaddingLevelSerde> for PaddingLevel {
    type Error = InvalidPaddingLevel;

    fn try_from(pls: PaddingLevelSerde) -> Result<PaddingLevel, Self::Error> {
        Ok(match pls {
            PaddingLevelSerde::String(s) => {
                s.as_ref().try_into().map_err(|_| InvalidPaddingLevel {})?
            }
            PaddingLevelSerde::Bool(false) => PaddingLevel::None,
            PaddingLevelSerde::Bool(true) => PaddingLevel::Normal,
        })
    }
}

/// Specification of (possibly) something to listen on (eg, a port, or some addresses/ports)
///
/// Can represent, at least:
///  * "do not listen"
///  * Listen on the following port on localhost (IPv6 and IPv4)
///  * Listen on precisely the following address and port
///  * Listen on several addresses/ports
///
/// Currently only IP (v6 and v4) is supported.
#[derive(Clone, Hash, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ListenSerde", into = "ListenSerde")]
#[derive(Default)]
pub struct Listen(Vec<ListenItem>);

impl Listen {
    /// Create a new `Listen` specifying no addresses (no listening)
    pub fn new_none() -> Listen {
        Listen(vec![])
    }

    /// Create a new `Listen` specifying listening on a port on localhost
    ///
    /// Special case: if `port` is zero, specifies no listening.
    pub fn new_localhost(port: u16) -> Listen {
        Listen(
            port.try_into()
                .ok()
                .map(ListenItem::Localhost)
                .into_iter()
                .collect_vec(),
        )
    }

    /// Create a new `Listen`, possibly specifying listening on a port on localhost
    ///
    /// Special case: if `port` is `Some(0)`, also specifies no listening.
    pub fn new_localhost_optional(port: Option<u16>) -> Listen {
        Self::new_localhost(port.unwrap_or_default())
    }

    /// Return true if no listening addresses have been configured
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// List the network socket addresses to listen on
    ///
    /// Each returned item is a list of `SocketAddr`,
    /// of which *at least one* must be successfully bound.
    /// It is OK if the others (up to all but one of them)
    /// fail with `EAFNOSUPPORT` ("Address family not supported").
    /// This allows handling of support, or non-support,
    /// for particular address families, eg IPv6 vs IPv4 localhost.
    /// Other errors (eg, `EADDRINUSE`) should always be treated as serious problems.
    ///
    /// Fails if the listen spec involves listening on things other than IP addresses.
    /// (Currently that is not possible.)
    pub fn ip_addrs(
        &self,
    ) -> Result<
        impl Iterator<Item = impl Iterator<Item = net::SocketAddr> + '_> + '_,
        ListenUnsupported,
    > {
        Ok(self.0.iter().map(|i| i.iter()))
    }

    /// Get the localhost port to listen on
    ///
    /// Returns `None` if listening is configured to be disabled.
    ///
    /// Fails, giving an unsupported error, if the configuration
    /// isn't just "listen on a single localhost port in all address families"
    pub fn localhost_port_legacy(&self) -> Result<Option<u16>, ListenUnsupported> {
        use ListenItem as LI;
        Ok(match &*self.0 {
            [] => None,
            [LI::Localhost(port)] => Some((*port).into()),
            _ => return Err(ListenUnsupported {}),
        })
    }
}

impl Display for Listen {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut sep = "";
        for a in &self.0 {
            write!(f, "{sep}{a}")?;
            sep = ", ";
        }
        Ok(())
    }
}
/// [`Listen`] configuration specified something not supported by application code
#[derive(thiserror::Error, Debug, Clone)]
#[non_exhaustive]
#[error("Unsupported listening configuration")]
pub struct ListenUnsupported {}

/// One item in the `Listen`
///
/// We distinguish `Localhost`,
/// rather than just storing two `net:SocketAddr`,
/// so that we can handle localhost (which means two address families) specially
/// in order to implement `localhost_port_legacy()`.
#[derive(Clone, Hash, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum ListenItem {
    /// One port, both IPv6 and IPv4
    Localhost(NonZeroU16),

    /// Any other single socket address
    General(net::SocketAddr),
}

impl ListenItem {
    /// Return the `SocketAddr`s implied by this item
    fn iter(&self) -> impl Iterator<Item = net::SocketAddr> + '_ {
        use net::{IpAddr, Ipv4Addr, Ipv6Addr};
        use ListenItem as LI;
        match self {
            &LI::Localhost(port) => Either::Left({
                let port = port.into();
                let addrs: [IpAddr; 2] = [Ipv6Addr::LOCALHOST.into(), Ipv4Addr::LOCALHOST.into()];
                addrs
                    .into_iter()
                    .map(move |ip| net::SocketAddr::new(ip, port))
            }),
            LI::General(addr) => Either::Right(iter::once(addr).cloned()),
        }
    }
}

impl Display for ListenItem {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ListenItem::Localhost(port) => write!(f, "localhost port {}", port)?,
            ListenItem::General(addr) => write!(f, "{}", addr)?,
        }
        Ok(())
    }
}
/// How we (de) serialize a [`Listen`]
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ListenSerde {
    /// for `listen = false` (in TOML syntax)
    Bool(bool),

    /// A bare item
    One(ListenItemSerde),

    /// An item in a list
    List(Vec<ListenItemSerde>),
}

/// One item in the list of a list-ish `Listen`, or the plain value
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ListenItemSerde {
    /// An integer.
    ///
    /// When appearing "loose" (in ListenSerde::One), `0` is parsed as none.
    Port(u16),

    /// An string which will be parsed as an address and port
    ///
    /// When appearing "loose" (in ListenSerde::One), `""` is parsed as none.
    String(String),
}

// This implementation isn't fallible, but clippy thinks it is because of the unwrap.
// The unwrap is just there because we can't pattern-match on a Vec
#[allow(clippy::fallible_impl_from)]
impl From<Listen> for ListenSerde {
    fn from(l: Listen) -> ListenSerde {
        let l = l.0;
        match l.len() {
            0 => ListenSerde::Bool(false),
            1 => ListenSerde::One(l.into_iter().next().expect("len=1 but no next").into()),
            _ => ListenSerde::List(l.into_iter().map(Into::into).collect()),
        }
    }
}
impl From<ListenItem> for ListenItemSerde {
    fn from(i: ListenItem) -> ListenItemSerde {
        use ListenItem as LI;
        use ListenItemSerde as LIS;
        match i {
            LI::Localhost(port) => LIS::Port(port.into()),
            LI::General(addr) => LIS::String(addr.to_string()),
        }
    }
}

/// Listen configuration is invalid
#[derive(thiserror::Error, Debug, Clone)]
#[non_exhaustive]
pub enum InvalidListen {
    /// Bool was `true` but that's not an address.
    #[error("Invalid listen specification: need actual addr/port, or `false`; not `true`")]
    InvalidBool,

    /// Specified listen was a string but couldn't parse to a [`net::SocketAddr`].
    #[error("Invalid listen specification: failed to parse string: {0}")]
    InvalidString(#[from] net::AddrParseError),

    /// Specified listen was a list containing a zero integer
    #[error("Invalid listen specification: zero (for no port) not permitted in list")]
    ZeroPortInList,
}
impl TryFrom<ListenSerde> for Listen {
    type Error = InvalidListen;

    fn try_from(l: ListenSerde) -> Result<Listen, Self::Error> {
        use ListenSerde as LS;
        Ok(Listen(match l {
            LS::Bool(false) => vec![],
            LS::Bool(true) => return Err(InvalidListen::InvalidBool),
            LS::One(i) if i.means_none() => vec![],
            LS::One(i) => vec![i.try_into()?],
            LS::List(l) => l.into_iter().map(|i| i.try_into()).try_collect()?,
        }))
    }
}
impl ListenItemSerde {
    /// Is this item actually a sentinel, meaning "don't listen, disable this thing"?
    ///
    /// Allowed only bare, not in a list.
    fn means_none(&self) -> bool {
        use ListenItemSerde as LIS;
        match self {
            &LIS::Port(port) => port == 0,
            LIS::String(s) => s.is_empty(),
        }
    }
}
impl TryFrom<ListenItemSerde> for ListenItem {
    type Error = InvalidListen;

    fn try_from(i: ListenItemSerde) -> Result<ListenItem, Self::Error> {
        use ListenItem as LI;
        use ListenItemSerde as LIS;
        Ok(match i {
            LIS::String(s) => LI::General(s.parse()?),
            LIS::Port(p) => LI::Localhost(p.try_into().map_err(|_| InvalidListen::ZeroPortInList)?),
        })
    }
}

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::mixed_attributes_style)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->
    use super::*;

    #[derive(Debug, Default, Deserialize, Serialize)]
    struct TestConfigFile {
        #[serde(default)]
        something_enabled: BoolOrAuto,

        #[serde(default)]
        padding: PaddingLevel,

        #[serde(default)]
        listen: Option<Listen>,

        #[serde(default)]
        auto_or_usize: ExplicitOrAuto<usize>,

        #[serde(default)]
        auto_or_bool: ExplicitOrAuto<bool>,
    }

    #[test]
    fn bool_or_auto() {
        use BoolOrAuto as BoA;

        let chk = |pl, s| {
            let tc: TestConfigFile = toml::from_str(s).expect(s);
            assert_eq!(pl, tc.something_enabled, "{:?}", s);
        };

        chk(BoA::Auto, "");
        chk(BoA::Auto, r#"something_enabled = "auto""#);
        chk(BoA::Explicit(true), r#"something_enabled = true"#);
        chk(BoA::Explicit(true), r#"something_enabled = "true""#);
        chk(BoA::Explicit(false), r#"something_enabled = false"#);
        chk(BoA::Explicit(false), r#"something_enabled = "false""#);

        let chk_e = |s| {
            let tc: Result<TestConfigFile, _> = toml::from_str(s);
            let _ = tc.expect_err(s);
        };

        chk_e(r#"something_enabled = 1"#);
        chk_e(r#"something_enabled = "unknown""#);
        chk_e(r#"something_enabled = "True""#);
    }

    #[test]
    fn padding_level() {
        use PaddingLevel as PL;

        let chk = |pl, s| {
            let tc: TestConfigFile = toml::from_str(s).expect(s);
            assert_eq!(pl, tc.padding, "{:?}", s);
        };

        chk(PL::None, r#"padding = "none""#);
        chk(PL::None, r#"padding = false"#);
        chk(PL::Reduced, r#"padding = "reduced""#);
        chk(PL::Normal, r#"padding = "normal""#);
        chk(PL::Normal, r#"padding = true"#);
        chk(PL::Normal, "");

        let chk_e = |s| {
            let tc: Result<TestConfigFile, _> = toml::from_str(s);
            let _ = tc.expect_err(s);
        };

        chk_e(r#"padding = 1"#);
        chk_e(r#"padding = "unknown""#);
        chk_e(r#"padding = "Normal""#);
    }

    #[test]
    fn listen_parse() {
        use net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        use ListenItem as LI;

        let localhost6 = |p| SocketAddr::new(Ipv6Addr::LOCALHOST.into(), p);
        let localhost4 = |p| SocketAddr::new(Ipv4Addr::LOCALHOST.into(), p);
        let unspec6 = |p| SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), p);

        #[allow(clippy::needless_pass_by_value)] // we do this for consistency
        fn chk(
            exp_i: Vec<ListenItem>,
            exp_addrs: Result<Vec<Vec<SocketAddr>>, ()>,
            exp_lpd: Result<Option<u16>, ()>,
            s: &str,
        ) {
            let tc: TestConfigFile = toml::from_str(s).expect(s);
            let ll = tc.listen.unwrap();
            eprintln!("s={:?} ll={:?}", &s, &ll);
            assert_eq!(ll, Listen(exp_i));
            assert_eq!(
                ll.ip_addrs()
                    .map(|a| a.map(|l| l.collect_vec()).collect_vec())
                    .map_err(|_| ()),
                exp_addrs
            );
            assert_eq!(ll.localhost_port_legacy().map_err(|_| ()), exp_lpd);
        }

        let chk_err = |exp, s: &str| {
            let got: Result<TestConfigFile, _> = toml::from_str(s);
            let got = got.expect_err(s).to_string();
            assert!(got.contains(exp), "s={:?} got={:?} exp={:?}", s, got, exp);
        };

        let chk_none = |s: &str| {
            chk(vec![], Ok(vec![]), Ok(None), &format!("listen = {}", s));
            chk_err(
                "", /* any error will do */
                &format!("listen = [ {} ]", s),
            );
        };

        let chk_1 = |v: ListenItem, addrs: Vec<Vec<SocketAddr>>, port, s| {
            chk(
                vec![v.clone()],
                Ok(addrs.clone()),
                port,
                &format!("listen = {}", s),
            );
            chk(
                vec![v.clone()],
                Ok(addrs.clone()),
                port,
                &format!("listen = [ {} ]", s),
            );
            chk(
                vec![v, LI::Localhost(23.try_into().unwrap())],
                Ok([addrs, vec![vec![localhost6(23), localhost4(23)]]]
                    .into_iter()
                    .flatten()
                    .collect()),
                Err(()),
                &format!("listen = [ {}, 23 ]", s),
            );
        };

        chk_none(r#""""#);
        chk_none(r#"0"#);
        chk_none(r#"false"#);
        chk(vec![], Ok(vec![]), Ok(None), r#"listen = []"#);

        chk_1(
            LI::Localhost(42.try_into().unwrap()),
            vec![vec![localhost6(42), localhost4(42)]],
            Ok(Some(42)),
            "42",
        );
        chk_1(
            LI::General(unspec6(56)),
            vec![vec![unspec6(56)]],
            Err(()),
            r#""[::]:56""#,
        );

        let chk_err_1 = |e, el, s| {
            chk_err(e, &format!("listen = {}", s));
            chk_err(el, &format!("listen = [ {} ]", s));
            chk_err(el, &format!("listen = [ 23, {}, 77 ]", s));
        };

        chk_err_1("need actual addr/port", "did not match any variant", "true");
        chk_err("did not match any variant", r#"listen = [ [] ]"#);
    }

    #[test]
    fn display_listen() {
        let empty = Listen::new_none();
        assert_eq!(empty.to_string(), "");

        let one_port = Listen::new_localhost(1234);
        assert_eq!(one_port.to_string(), "localhost port 1234");

        let multi_port = Listen(vec![
            ListenItem::Localhost(1111.try_into().unwrap()),
            ListenItem::Localhost(2222.try_into().unwrap()),
        ]);
        assert_eq!(
            multi_port.to_string(),
            "localhost port 1111, localhost port 2222"
        );

        let multi_addr = Listen(vec![
            ListenItem::Localhost(1234.try_into().unwrap()),
            ListenItem::General("1.2.3.4:5678".parse().unwrap()),
        ]);
        assert_eq!(multi_addr.to_string(), "localhost port 1234, 1.2.3.4:5678");
    }

    #[test]
    fn explicit_or_auto() {
        use ExplicitOrAuto as EOA;

        let chk = |eoa: EOA<usize>, s| {
            let tc: TestConfigFile = toml::from_str(s).expect(s);
            assert_eq!(
                format!("{:?}", eoa),
                format!("{:?}", tc.auto_or_usize),
                "{:?}",
                s
            );
        };

        chk(EOA::Auto, r#"auto_or_usize = "auto""#);
        chk(EOA::Explicit(20), r#"auto_or_usize = 20"#);

        let chk_e = |s| {
            let tc: Result<TestConfigFile, _> = toml::from_str(s);
            let _ = tc.expect_err(s);
        };

        chk_e(r#"auto_or_usize = """#);
        chk_e(r#"auto_or_usize = []"#);
        chk_e(r#"auto_or_usize = {}"#);

        let chk = |eoa: EOA<bool>, s| {
            let tc: TestConfigFile = toml::from_str(s).expect(s);
            assert_eq!(
                format!("{:?}", eoa),
                format!("{:?}", tc.auto_or_bool),
                "{:?}",
                s
            );
        };

        // ExplicitOrAuto<bool> works just like BoolOrAuto
        chk(EOA::Auto, r#"auto_or_bool = "auto""#);
        chk(EOA::Explicit(false), r#"auto_or_bool = false"#);

        chk_e(r#"auto_or_bool= "not bool or auto""#);

        let mut config = TestConfigFile::default();
        let toml = toml::to_string(&config).unwrap();
        assert_eq!(
            toml,
            r#"something_enabled = "auto"
padding = "normal"
auto_or_usize = "auto"
auto_or_bool = "auto"
"#
        );

        config.auto_or_bool = ExplicitOrAuto::Explicit(true);
        let toml = toml::to_string(&config).unwrap();
        assert_eq!(
            toml,
            r#"something_enabled = "auto"
padding = "normal"
auto_or_usize = "auto"
auto_or_bool = true
"#
        );
    }
}
