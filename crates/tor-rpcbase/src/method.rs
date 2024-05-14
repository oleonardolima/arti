//! Method type for the RPC system.

use std::collections::HashSet;

use derive_deftly::define_derive_deftly;
use downcast_rs::Downcast;
use once_cell::sync::Lazy;

/// The parameters and method name associated with a given Request.
///
/// Use [`derive_deftly(DynMethod)`](derive_deftly_template_DynMethod)
/// for a template to declare one of these.
///
/// # Note
///
/// In order to comply with our spec, all Methods' data must be represented as a json
/// object.
//
// TODO RPC: Possible issue here is that, if this trait is public, anybody outside
// of Arti can use this trait to add new methods to the RPC engine. Should we
// care?
pub trait DynMethod: std::fmt::Debug + Send + Downcast {}
downcast_rs::impl_downcast!(DynMethod);

/// A DynMethod that can be deserialized.
///
/// We use [`typetag`] here so that we define `Method`s in other crates.
///
/// Use [`derive_deftly(DynMethod)`](derive_deftly_template_DynMethod)
/// for a template to declare one of these.
#[typetag::deserialize(tag = "method", content = "params")]
pub trait DeserMethod: DynMethod {
    /// Up-cast to a `Box<dyn DynMethod>`.
    fn upcast_box(self: Box<Self>) -> Box<dyn DynMethod>;
}

/// A typed method, used to ensure that all implementations of a method have the
/// same success and updates types.
///
/// Prefer to implement this trait, rather than `DynMethod` or `DeserMethod`.
/// (Those traits represent a type-erased method, with statically-unknown `Output` and
/// `Update` types.)
///
/// All Methods can be invoked via `DispatchTable::invoke_special`.
/// To be invoked from the RPC system, a methods associated `Output` and `Update` types
/// must additionally implement `Serialize`, and its `Error` type must implement
/// `Into<RpcError>`
pub trait Method: DynMethod {
    /// A type returned by this method on success.
    type Output: Send + 'static;
    /// A type sent by this method on updates.
    ///
    /// If this method will never send updates, use the uninhabited
    /// [`NoUpdates`] type.
    type Update: Send + 'static;
    /// A type returned by this method on failure.
    //
    // TODO: I'd like this to default to RpcError, but defaulting isn't implemented.
    type Error: Send + 'static;
}

/// An uninhabited type, used to indicate that a given method will never send
/// updates.
#[derive(serde::Serialize)]
#[allow(clippy::exhaustive_enums)]
pub enum NoUpdates {}

/// A method we're registering.
///
/// This struct's methods are public so it can be constructed from
/// `decl_method!`.
///
/// If you construct it yourself, you'll be in trouble.  But you already knew
/// that, since you're looking at a `doc(hidden)` thing.
#[doc(hidden)]
#[allow(clippy::exhaustive_structs)]
pub struct MethodInfo_ {
    /// The name of the method.
    pub method_name: &'static str,
}

inventory::collect!(MethodInfo_);

define_derive_deftly! {
/// Declare that one or more space-separated types should be considered
/// as dynamically dispatchable RPC methods.
///
/// # Example
///
/// ```
/// use tor_rpcbase::{self as rpc, templates::*};
/// use derive_deftly::Deftly;
///
/// #[derive(Debug, serde::Deserialize, Deftly)]
/// #[derive_deftly(rpc::DynMethod)]
/// #[deftly(rpc(method_name = "x-example:castigate"))]
/// struct Castigate {
///    severity: f64,
///    offenses: Vec<String>,
///    accomplice: Option<rpc::ObjectId>,
/// }
///
/// impl rpc::Method for Castigate {
///     type Output = String;
///     type Update = rpc::NoUpdates;
///     type Error = rpc::RpcError;
/// }
/// ```
    pub DynMethod =
    const _: () = {
        impl $crate::DynMethod for $ttype {}

        ${select1 tmeta(rpc(method_name)) {
            // Alas, `typetag does not work correctly when not in scope as `typetag`.
            use $crate::typetag;
            #[typetag::deserialize(name = ${tmeta(rpc(method_name)) as str})]
            // Note that we do not support generics in method types.
            // If we did, we would have to give each instantiation type its own method name.
            impl $crate::DeserMethod for $ttype {
                fn upcast_box(self: Box<Self>) -> Box<dyn $crate::DynMethod> {
                    self as _
                }
            }
            $crate::inventory::submit! {
                $crate::MethodInfo_ { method_name : ${tmeta(rpc(method_name)) as str} }
            }
        } else if tmeta(rpc(no_method_name)) {
            // don't derive DeserMethod.
        }}
    };
}
pub use derive_deftly_template_DynMethod;

/// Return true if `name` is the name of some method.
pub fn is_method_name(name: &str) -> bool {
    /// Lazy set of all method names.
    static METHOD_NAMES: Lazy<HashSet<&'static str>> = Lazy::new(|| iter_method_names().collect());
    METHOD_NAMES.contains(name)
}

/// Return an iterator that yields every registered method name.
///
/// Used (e.g.) to enforce syntactic requirements on method names.
pub fn iter_method_names() -> impl Iterator<Item = &'static str> {
    inventory::iter::<MethodInfo_>().map(|mi| mi.method_name)
}

/// Error representing an "invalid" method name.
#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidMethodName {
    /// The method doesn't have a ':' to demarcate its namespace.
    #[error("Method has no namespace separator")]
    NoNamespace,

    /// The method's namespace is not one we recognize.
    #[error("Method has unrecognized namespace")]
    UnrecognizedNamespace,

    /// The method's name is not in snake_case.
    #[error("Method name has unexpected format")]
    BadMethodName,
}

/// Check whether `method` is an expected and well-formed method name.
fn is_valid_method_name(
    recognized_namespaces: &HashSet<&str>,
    method: &str,
) -> Result<(), InvalidMethodName> {
    // Return true if scope is recognized.
    let scope_ok = |s: &str| s.starts_with("x-") || recognized_namespaces.contains(&s);
    /// Return true if name is in acceptable format.
    fn name_ok(n: &str) -> bool {
        let mut chars = n.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        first.is_ascii_lowercase()
            && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    }
    let (scope, name) = method
        .split_once(':')
        .ok_or(InvalidMethodName::NoNamespace)?;

    if !scope_ok(scope) {
        return Err(InvalidMethodName::UnrecognizedNamespace);
    }
    if !name_ok(name) {
        return Err(InvalidMethodName::BadMethodName);
    }

    Ok(())
}

/// Check whether we have any method names that do not conform to our conventions.
///
/// Violations of these conventions won't stop the RPC system from working, but they may result in
/// annoyances with namespacing, .
///
/// If provided, `additional_namespaces` is a list of namespaces other than our standard ones that
/// we should accept.
///
/// Returns a `Vec` of method names that violate our rules, along with the rules that they violate.
pub fn check_method_names<'a>(
    additional_namespaces: impl IntoIterator<Item = &'a str>,
) -> Vec<(&'static str, InvalidMethodName)> {
    let mut recognized_namespaces: HashSet<&str> = additional_namespaces.into_iter().collect();
    recognized_namespaces.extend(["arti", "rpc", "auth"]);

    iter_method_names()
        .filter_map(|name| {
            is_valid_method_name(&recognized_namespaces, name)
                .err()
                .map(|e| (name, e))
        })
        .collect()
}
