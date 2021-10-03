//! `tor-guardmgr`: guard node selection for Tor network clients.
//!
//! # Overview
//!
//! This crate is part of
//! [Arti](https://gitlab.torproject.org/tpo/core/arti/), a project to
//! implement [Tor](https://www.torproject.org/) in Rust.
//!
//! "Guard nodes" are mechanism that Tor clients uses to limit the
//! impact of hostile relays. Approximately: each client chooses a
//! small set of relays to use as its "guards".  Later, when the
//! client picks its paths through network, rather than choosing a
//! different first hop randomly for every path, it chooses the best
//! "guard" as the first hop.
//!
//! This crate provides [`GuardMgr`], an object that manages a set of
//! guard nodes, and helps the `tor-circmgr` crate know when to use
//! them.
//!
//! Guard nodes are persistent across multiple process invocations.
//!
//! More Arti users won't need to use this crate directly.
//!
//! # Motivation
//!
//! What's the point?  By restricting their first hops to a small set,
//! clients increase their odds against traffic-correlation attacks.
//! Since we assume that an adversary who controls both ends of a
//! circuit can correlate its traffic, choosing many circuits with
//! random entry points will eventually cause a client to eventually
//! pick an attacker-controlled circuit, with probability approaching
//! 1 over time.  If entry nodes are restricted to a small set,
//! however, then the client has a chance of never picking an
//! attacker-controlled circuit.
//!
//! (The actual argument is a little more complicated here, and it
//! relies on the assumption that, since the attacker knows
//! statistics, exposing _any_ of your traffic is nearly as bad as
//! exposing _all_ of your traffic.)
//!
//! # Complications
//!
//! The real algorithm for selecting and using guards can get more
//! complicated because of a variety of factors.
//!
//! - In reality, we can't just "pick a few guards at random" and use
//!   them forever: relays can appear and disappear, relays can go
//!   offline and come back online, and so on.  What's more, keeping
//!   guards for too long can make targeted attacks against those
//!   guards more attractive.
//!
//! - Further, we may have particular restrictions on where we can
//!   connect. (For example, we might be restricted to ports 80 and
//!   443, but only when we're on a commuter train's wifi network.)
//!
//! - We need to resist attacks from local networks that block all but a
//!   small set of guard relays, to force us to choose those.
//!
//! - We need to give good, reliable performance while using the
//!   guards that we prefer.
//!
//! These needs complicate our API somewhat.  Instead of simply asking
//! the `GuardMgr` for a guard, the circuit-management code needs to
//! be able to tell the `GuardMgr` that a given guard has failed (or
//! succeeded), and that it needs a different guard in the future (or
//! not).
//!
//! Further, the `GuardMgr` code needs to be able to hand out
//! _provisional guards_, in effect saying "You can try building a
//! circuit with this guard, but please don't actually _use_ that
//! circuit unless I tell you it's safe."
//!
//! For details on the exact algorithm, see `guard-spec.txt` (link
//! below) and comments and internal documentation in this crate.
//!
//! # References
//!
//! Guard nodes were first proposes (as "helper nodes") in "Defending
//! Anonymous Communications Against Passive Logging Attacks" by
//! Matther Wright, Micah Adler, Brian N. Levine, and Clay Shields in
//! the Proceedings of the 2003 IEEE Symposium on Security and
//! Privacy.  (See <https://www.freehaven.net/anonbib/#wright03>)
//!
//! Tor's current guard selection algorithm is described in Tor's
//! [`guard-spec.txt`](https://gitlab.torproject.org/tpo/core/torspec/-/raw/main/guard-spec.txt)
//! document.

// Glossary:
//     Primary guard
//     Sample
//     confirmed
//     filtered

#![deny(missing_docs)]
#![warn(noop_method_call)]
#![deny(unreachable_pub)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::cargo_common_metadata)]
#![deny(clippy::cast_lossless)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::cognitive_complexity)]
#![deny(clippy::debug_assert_with_mut_call)]
#![deny(clippy::exhaustive_enums)]
#![deny(clippy::exhaustive_structs)]
#![deny(clippy::expl_impl_clone_on_copy)]
#![deny(clippy::fallible_impl_from)]
#![deny(clippy::implicit_clone)]
#![deny(clippy::large_stack_arrays)]
#![warn(clippy::manual_ok_or)]
#![deny(clippy::missing_docs_in_private_items)]
#![deny(clippy::missing_panics_doc)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_pass_by_value)]
#![warn(clippy::option_option)]
#![warn(clippy::rc_buffer)]
#![deny(clippy::ref_option_ref)]
#![warn(clippy::trait_duplication_in_bounds)]
#![deny(clippy::unnecessary_wraps)]
#![warn(clippy::unseparated_literal_suffix)]
#![deny(clippy::unwrap_used)]

use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::task::{SpawnError, SpawnExt};
use futures::SinkExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tracing::warn;

use tor_llcrypto::pk;
use tor_netdir::{params::NetParameters, NetDir, Relay};
use tor_persist::{DynStorageHandle, StateMgr};
use tor_rtcompat::Runtime;

mod daemon;
mod filter;
mod guard;
mod pending;
mod sample;
mod util;

pub use filter::GuardFilter;
pub use pending::{GuardMonitor, GuardUsable};

use pending::{GuardStatusMsg, PendingRequest, RequestId};
use sample::{GuardSet, PickGuardError};

/// A "guard manager" that selects and remembers a persistent set of
/// guard nodes.
///
#[derive(Clone)]
pub struct GuardMgr<R: Runtime> {
    /// An asynchronous runtime object.
    ///
    /// GuardMgr uses this runtime for timing, timeouts, and spawning
    /// tasks.
    runtime: R,

    /// Internal state for the guard manager.
    // TODO: I wish I could use a regular mutex rather than a
    // futures::lock::Mutex, but I don't see how that's feasible.  We
    // need to get access to inner.ctrl and then send over it, which
    // means we need an async mutex.
    //
    // Conceivably, I could move ctrl out to GuardMgr, and then put it
    // under a sync::Mutex.  Is that smart?
    inner: Arc<Mutex<GuardMgrInner>>,
}

// TODO: Make the above type Debug.

/// Helper type that holds the data used by a [`GuardMgr`].
///
/// This would just be a [`GuardMgr`], except that it needs to sit inside
/// a `Mutex` and get accessed by daemon tasks.
struct GuardMgrInner {
    /// Last time when we've taken note of activity that means we're
    /// online.
    ///
    /// We use this timestamp when a low-priority guard is discovered
    /// to be online.  If it happens when we've been _offline_ for a
    /// while, then we retry our primary guards before using the low
    /// priority guard, so that we can be sure that they're really
    /// down (and didn't just _seem_ to be down because we were
    /// offline).
    last_time_on_internet: Option<Instant>,

    /// The currently active [`GuardSet`] object.
    ///
    /// This object remembers a persistent set of guards that we can use, along
    /// with their relative priorities and statuses.
    ///
    /// Right now, there's only one `GuardSet` per `GuardMgr`, but we
    /// expect that to change: our algorithm specifies that there can
    /// be multiple named guard sets, and we can swap between them
    /// depending on the user's selected [`GuardFilter`].
    active_guards: GuardSet,

    /// Configuration values derived from the consensus parameters.
    ///
    /// This is updated whenever the consensus parameters change.
    params: GuardParams,

    /// A mpsc channel, used to tell the task running in
    /// [`daemon::report_status_events`] about a new event to monitor.
    ctrl: mpsc::Sender<daemon::Msg>,

    /// Information about guards that we've given out, but where we have
    /// not yet heard whether the guard was successful.
    ///
    /// Upon leaning whether the guard was successful, the pending
    /// requests in this map may be either moved to `waiting`, or
    /// discarded.
    ///
    /// There can be multiple pending requests corresponding to the
    /// same guard.
    pending: HashMap<RequestId, PendingRequest>,

    /// A list of pending requests for which we have heard that the
    /// guard was successful, but we have not yet decided whether the
    /// circuit may be used.
    ///
    /// There can be multiple waiting requests corresponding to the
    /// same guard.
    waiting: Vec<PendingRequest>,

    /// Location in which to store persistent state.
    ///
    /// (This is only the state for the default set of guards.)
    default_storage: DynStorageHandle<GuardSet>,
}

impl<R: Runtime> GuardMgr<R> {
    /// Create a new "empty" guard manager and launch its background tasks.
    ///
    /// It won't be able to hand out any guards until
    /// [`GuardMgr::update_network`] has been called.
    ///
    /// # Limitations
    pub fn new<S>(runtime: R, state_mgr: S) -> Result<Self, SpawnError>
    where
        S: StateMgr + Send + Sync + 'static,
    {
        let (ctrl, rcv) = mpsc::channel(32);
        let default_storage = state_mgr.create_handle("default_guards");
        let active_guards = default_storage
            .load()
            .expect("Load error") //XXXX propagate this!!!
            .unwrap_or_else(GuardSet::new);
        let inner = Arc::new(Mutex::new(GuardMgrInner {
            active_guards,
            last_time_on_internet: None,
            params: GuardParams::default(),
            ctrl,
            pending: HashMap::new(),
            waiting: Vec::new(),
            default_storage,
        }));
        {
            let weak_inner = Arc::downgrade(&inner);
            let rt_clone = runtime.clone();
            runtime.spawn(daemon::report_status_events(rt_clone, weak_inner, rcv))?;
        }
        {
            let rt_clone = runtime.clone();
            let weak_inner = Arc::downgrade(&inner);
            runtime.spawn(daemon::run_periodic(rt_clone, weak_inner))?;
        }
        Ok(GuardMgr { runtime, inner })
    }

    /// Flush our current guard state to the state manager, if there
    /// is any unsaved state.
    pub async fn update_persistent_state(&self) -> Result<(), tor_persist::Error> {
        let inner = self.inner.lock().await;
        let _ignore = inner.default_storage.try_lock()?; // TODO: Don't ignore.
        inner.default_storage.store(&inner.active_guards)?;
        Ok(())
    }

    /// Update the state of this [`GuardMgr`] based on a new or modified
    /// [`NetDir`] object.
    ///
    /// This method can add new guards, or notice that existing guards
    /// have become unusable.  It needs a `NetDir` so it can identify
    /// potential candidate guards.
    ///
    /// Call this method whenever the `NetDir` changes.
    pub async fn update_network(&self, netdir: &NetDir) {
        let now = self.runtime.wallclock();

        let mut inner = self.inner.lock().await;

        inner.update(now, Some(netdir));
    }

    /// Record that some internet activity has happened that tells us
    /// we're online.
    ///
    /// We can use this information to determine when we should retry
    /// our primary guards on the basis of having been down for a long
    /// time.
    ///
    /// # Limitations
    ///
    /// We should really callthis every time we read a cell, but that
    /// isn't efficient or practical.  We'll probably have to refactor
    /// things somehow. (TODO)
    pub async fn note_internet_activity(&self) {
        let now = self.runtime.now();
        let mut inner = self.inner.lock().await;
        inner.last_time_on_internet = Some(now);
    }

    /// Replace the current [`GuardFilter`] used by this `GuardMgr`.
    ///
    /// (Since there is only one kind of filter right now, there's no
    /// real reason to call this function, but at least it should work.
    pub async fn set_filter(&self, filter: GuardFilter, netdir: &NetDir) {
        // First we have to see how much of the possible guard space
        // this new filter allows.  (We don't use this info yet, but we will
        // one we have nontrivial filters.)
        let n_guards = netdir.relays().filter(|r| r.is_flagged_guard()).count();
        let n_permitted = netdir
            .relays()
            .filter(|r| r.is_flagged_guard() && filter.permits(r))
            .count();
        let frac_permitted = if n_guards > 0 {
            n_permitted as f64 / (n_guards as f64)
        } else {
            1.0
        };

        let now = self.runtime.wallclock();
        let mut inner = self.inner.lock().await;

        let restrictive_filter = frac_permitted < inner.params.filter_threshold;

        // TODO: Once we support nontrivial filters, we might have to
        // swap out "active_guards" depending on which set it is.
        // TODO: Warn if the filter is waaaay to small according to guard params.

        inner.active_guards.set_filter(filter, restrictive_filter);
        inner.update(now, Some(netdir));
    }

    /// Select a guard for a given [`GuardUsage`].
    ///
    /// On success, we return a [`GuardId`] object to identify which
    /// guard we have picked, a [`GuardMonitor`] object that the
    /// caller can use to report whether its attempt to use the guard
    /// succeeded or failed, and a [`GuardUsable`] future that the
    /// caller can use to decide whether a circuit built through the
    /// guard is actually safe to use.
    ///
    /// That last point is important: It's okay to build a circuit
    /// through the guard returned by this function, but you can't
    /// actually use it for traffic unless the [`GuardUsable`] future
    /// yields "true".
    ///
    /// # Limitations
    ///
    /// This function will never return a guard that isn't listed in
    /// the [`NetDir`] most recently passed to [`GuardMgr::update_network`].
    /// That's _usually_ what you'd want, but when we're trying to
    /// bootstrap we might want to use _all_ guards as possible
    /// directory caches.  That's not implemented yet.
    pub async fn select_guard(
        &self,
        usage: GuardUsage,
        netdir: Option<&NetDir>,
    ) -> Result<(GuardId, GuardMonitor, GuardUsable), PickGuardError> {
        // XXXX: Does this need to take a NetDir?
        let now = self.runtime.now();
        let wallclock = self.runtime.wallclock();

        let mut inner = self.inner.lock().await;
        // XXXX: need to add more stuff here?

        // XXXX Really have to do this?
        inner.active_guards.consider_all_retries(now);

        let (origin, guard_id) = inner.select_guard_with_retries(&usage, netdir, wallclock)?;

        let (usable, usable_sender) = if origin.is_primary() {
            (GuardUsable::new_primary(), None)
        } else {
            let (u, snd) = GuardUsable::new_uncertain();
            (u, Some(snd))
        };
        let (monitor, rcv) = GuardMonitor::new();

        let request_id = pending::RequestId::next();
        let pending_request =
            pending::PendingRequest::new(guard_id.clone(), usage, usable_sender, now);
        inner.pending.insert(request_id, pending_request);

        inner.active_guards.record_attempt(&guard_id, now);

        // Have to do this while not holding lock, since it awaits.
        // TODO: I wish this function didn't have to be async.
        inner
            .ctrl
            .send(daemon::Msg::Observe(request_id, rcv))
            .await
            .expect("Guard observer task exited prematurely");

        Ok((guard_id, monitor, usable))
    }
}

impl GuardMgrInner {
    /// Update the status of all guards in the active set, based on
    /// the passage of time and (optionally) a network directory.
    ///
    /// We can expire guards based on the time alone; we can only
    /// add guards or change their status with a NetDir.
    fn update(&mut self, now: SystemTime, netdir: Option<&NetDir>) {
        // Set the parameters.
        if let Some(netdir) = netdir {
            match GuardParams::try_from(netdir.params()) {
                Ok(params) => self.params = params,
                Err(e) => warn!("Unusable guard parameters from consensus: {}", e),
            }
        }

        // Then expire guards.  Do that early, in case we need more.
        self.active_guards.expire_old_guards(&self.params, now);

        if let Some(netdir) = netdir {
            self.active_guards.update_status_from_netdir(netdir);
            loop {
                let added_any =
                    self.active_guards
                        .extend_sample_as_needed(now, &self.params, netdir);
                if !added_any {
                    break;
                }
            }
        }

        self.active_guards.select_primary_guards(&self.params);
    }

    /// Called when the circuit manager reports (via [`GuardMonitor`]) that
    /// a guard succeeded or failed.
    ///
    /// Changes the guard's status as appropriate, and updates the pending
    /// request as needed.
    pub(crate) fn handle_msg(
        &mut self,
        request_id: RequestId,
        status: GuardStatusMsg,
        runtime: &impl tor_rtcompat::SleepProvider,
    ) {
        if let Some(mut pending) = self.pending.remove(&request_id) {
            // If there was a pending request matching this RequestId, great!
            let guard_id = pending.guard_id();
            match status {
                GuardStatusMsg::Success => {
                    let now = runtime.now();
                    // If we've been gone too long without any net activitity,
                    // and now we're seeing a circuit succeed,
                    // tell the primary guards that they might be retriable.
                    if let Some(last_time) = self.last_time_on_internet {
                        let dur = now.saturating_duration_since(last_time);
                        // TODO: we should use the actual timeout for this, but
                        // we can't do it yet, since we don't have anything reliable
                        // calling note_internet_activity.
                        // let timeout = self.params.internet_down_timeout;
                        let timeout = Duration::from_secs(7200); // (Fake timeout)
                        if dur >= timeout {
                            self.active_guards.mark_primary_guards_retriable();
                        }
                    }
                    self.last_time_on_internet = Some(now);

                    // The guard succeeded.  Tell the GuardSet.
                    self.active_guards
                        .record_success(guard_id, &self.params, runtime.wallclock());
                    // Either tell the request whether the guard is
                    // usable, or schedule it as a "waiting" request.
                    if let Some(usable) = self.guard_usability_status(&pending, runtime.now()) {
                        pending.reply(usable);
                    } else {
                        // This is the one case where we can't use the
                        // guard yet.
                        pending.mark_waiting(runtime.now());
                        self.waiting.push(pending);
                    }
                }
                GuardStatusMsg::Failure => {
                    self.active_guards.record_failure(guard_id, runtime.now());
                    pending.reply(false);
                }
                GuardStatusMsg::AttemptAbandoned => {
                    self.active_guards.record_attempt_abandoned(guard_id);
                    pending.reply(false);
                }
            };
        } else {
            warn!(
                "Got a status {:?} for a request {:?} that wasn't pending",
                status, request_id
            );
        }

        // We might need to update the primary guards based on changes in the
        // status of guards above.
        self.active_guards.select_primary_guards(&self.params);

        // Some waiting request may just have become ready (usable or
        // not); we need to give them the information they're waiting
        // for.
        self.expire_and_answer_pending_requests(runtime.now());
    }

    /// If the circuit built because of a given [`PendingRequest`] may
    /// now be used (or discarded), return `Some(true)` or
    /// `Some(false)` respectively.
    ///
    /// Return None if we can't yet give an answer about whether such
    /// a circuit is usable.
    fn guard_usability_status(&self, pending: &PendingRequest, now: Instant) -> Option<bool> {
        // XXXX This isn't really what the spec says. The original
        // spec describes this rule in terms of other circuits, not in
        // terms of other guards.  I think this is a better algorithm,
        // though, and doesn't require us to look at circuits or at
        // other requests.
        self.active_guards.circ_usability_status(
            pending.guard_id(),
            pending.usage(),
            &self.params,
            now,
        )
    }

    /// For requests that have been "waiting" for an answer for too long,
    /// expire them and tell the circuit manager that their circuits
    /// are unusable.
    fn expire_and_answer_pending_requests(&mut self, now: Instant) {
        // TODO: Use Vec::drain_filter when/if it's stable.
        use retain_mut::RetainMut;

        // A bit ugly: we use a separate Vec here to avoid borrowing issues,
        // and put it back when we're done.
        let mut waiting = Vec::new();
        std::mem::swap(&mut waiting, &mut self.waiting);

        waiting.retain_mut(|pending| {
            let expired = pending
                .waiting_since()
                .and_then(|w| now.checked_duration_since(w))
                .map(|d| d >= self.params.np_idle_timeout)
                == Some(true);
            if expired {
                pending.reply(false);
                return false;
            }
            // XXXX: guard_usability_status isn't what the spec says.  It
            // says instead that we should look at _circuit_ status, saying:
            //  "   Definition: In the algorithm above, C2 "blocks" C1 if:
            // * C2 obeys all the restrictions that C1 had to obey, AND
            // * C2 has higher priority than C1, AND
            // * Either C2 is <complete>, or C2 is <waiting_for_better_guard>,
            // or C2 has been <usable_if_no_better_guard> for no more than
            // {NONPRIMARY_GUARD_CONNECT_TIMEOUT} seconds."

            if let Some(answer) = self.guard_usability_status(pending, now) {
                pending.reply(answer);
                return false;
            }
            true
        });

        // Put the waiting list back.
        std::mem::swap(&mut waiting, &mut self.waiting);
    }

    /// Run any periodic events that update guard status, and return a
    /// duration after which periodic events should next be run.
    pub(crate) fn run_periodic_events(&mut self, wallclock: SystemTime, now: Instant) -> Duration {
        // TODO: maybe this needs to take a netdir :p

        self.update(wallclock, None);
        self.expire_and_answer_pending_requests(now);
        // XXXX Anything else?
        Duration::from_secs(1) // TODO: Too aggressive.
    }

    /// Try to select a guard, expanding the sample or marking guards retriable
    /// if the first attempts fail.
    fn select_guard_with_retries(
        &mut self,
        usage: &GuardUsage,
        netdir: Option<&NetDir>,
        now: SystemTime,
    ) -> Result<(sample::ListKind, GuardId), PickGuardError> {
        // Try to find a guard.
        if let Ok(s) = self.active_guards.pick_guard(usage, &self.params) {
            return Ok(s);
        }

        // That didn't work. If we have a netdir, expand the sample and try again.
        if let Some(dir) = netdir {
            if self
                .active_guards
                .extend_sample_as_needed(now, &self.params, dir)
            {
                self.active_guards.select_primary_guards(&self.params);
                if let Ok(s) = self.active_guards.pick_guard(usage, &self.params) {
                    return Ok(s);
                }
            }
        }

        // That didn't work either. Mark everybody as potentially retriable.
        self.active_guards.mark_all_guards_retriable();
        self.active_guards.pick_guard(usage, &self.params)
    }
}

/// A set of parameters, derived from the consensus document, controlling
/// the behavior of a guard manager.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
struct GuardParams {
    /// How long should a sampled, un-confirmed guard be kept in the sample before it expires?
    lifetime_unconfirmed: Duration,
    /// How long should a confirmed guard be kept in the sample before
    /// it expires?
    lifetime_confirmed: Duration,
    /// How long may  a guard be unlisted before we remove it from the sample?
    lifetime_unlisted: Duration,
    /// Largest number of guards we're willing to add to the sample.
    max_sample_size: usize,
    /// Largest fraction of the network's guard bandwidth that we're
    /// willing to add to the sample.
    max_sample_bw_fraction: f64,
    /// Smallest number of guards that we're willing to have in the
    /// sample, after applying a [`GuardFilter`].
    min_filtered_sample_size: usize,
    /// How many guards are considered "Primary"?
    n_primary: usize,
    /// When making a regular circuit, how many primary guards should we
    /// be willing to try?
    data_parallelism: usize,
    /// When making a one-hop directory circuit, how many primary
    /// guards should we be willing to try?
    dir_parallelism: usize,
    /// For how long does a pending attempt to connect to a guard
    /// block an attempt to use a less-favored non-primary guard?
    np_connect_timeout: Duration,
    /// How long do we allow a circuit to a successful but unfavored
    /// non-primary guard to sit around before deciding not to use it?
    np_idle_timeout: Duration,
    /// After how much time without successful activity does a
    /// successful circuit indicate that we should retry our primary
    /// guards?
    internet_down_timeout: Duration,
    /// What fraction of the guards can be can be filtered out before we
    /// decide that our filter is "very restrictive"?
    ///
    /// (Not fully implemented yet.)
    filter_threshold: f64,
}

impl Default for GuardParams {
    fn default() -> Self {
        let one_day = Duration::from_secs(86400);
        GuardParams {
            lifetime_unconfirmed: one_day * 120,
            lifetime_confirmed: one_day * 60,
            lifetime_unlisted: one_day * 20,
            max_sample_size: 60,
            max_sample_bw_fraction: 0.2,
            min_filtered_sample_size: 20,
            n_primary: 3,
            data_parallelism: 1,
            dir_parallelism: 3,
            np_connect_timeout: Duration::from_secs(15),
            np_idle_timeout: Duration::from_secs(600),
            internet_down_timeout: Duration::from_secs(600),
            filter_threshold: 0.2,
        }
    }
}

impl TryFrom<&NetParameters> for GuardParams {
    type Error = tor_units::Error;
    fn try_from(p: &NetParameters) -> Result<GuardParams, Self::Error> {
        Ok(GuardParams {
            lifetime_unconfirmed: p.guard_lifetime_unconfirmed.try_into()?,
            lifetime_confirmed: p.guard_lifetime_confirmed.try_into()?,
            lifetime_unlisted: p.guard_remove_unlisted_after.try_into()?,
            max_sample_size: p.guard_max_sample_size.try_into()?,
            max_sample_bw_fraction: p.guard_max_sample_threshold.as_fraction(),
            min_filtered_sample_size: p.guard_filtered_min_sample_size.try_into()?,
            n_primary: p.guard_n_primary.try_into()?,
            data_parallelism: p.guard_use_parallelism.try_into()?,
            dir_parallelism: p.guard_dir_use_parallelism.try_into()?,
            np_connect_timeout: p.guard_nonprimary_connect_timeout.try_into()?,
            np_idle_timeout: p.guard_nonprimary_idle_timeout.try_into()?,
            internet_down_timeout: p.guard_internet_likely_down.try_into()?,
            filter_threshold: p.guard_meaningful_restriction.as_fraction(),
        })
    }
}

/// A unique cryptographic identifier for a selected guard.
///
/// (This is implemented internally using both of the guard's Ed25519
/// and RSA identities.)
// TODO: should we move this structure?
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub struct GuardId {
    /// Ed25519 identity key for a a guard
    ed25519: pk::ed25519::Ed25519Identity,
    /// RSA identity fingerprint for a a guard
    rsa: pk::rsa::RsaIdentity,
}

impl GuardId {
    /// Return a new, manually constructed GuardId
    fn new(ed25519: pk::ed25519::Ed25519Identity, rsa: pk::rsa::RsaIdentity) -> Self {
        Self { ed25519, rsa }
    }

    /// Extract a GuardId from a Relay object.
    pub(crate) fn from_relay(relay: &tor_netdir::Relay<'_>) -> Self {
        Self::new(*relay.id(), *relay.rsa_id())
    }

    /// Return the relay in `netdir` that corresponds to this ID, if there
    /// is one.
    pub fn get_relay<'a>(&self, netdir: &'a NetDir) -> Option<Relay<'a>> {
        netdir.by_id_pair(&self.ed25519, &self.rsa)
    }
}

/// The purpose for which we plan to use a guard.
///
/// This can affect the guard selection algorithm.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GuardUsageKind {
    /// We want to use this guard for a data circuit.
    ///
    /// (This encompasses everything except the `OneHopDirectory` case.)
    Data,
    /// We want to use this guard for a one-hop, non-anonymous
    /// directory request.
    ///
    /// (Our algorithm allows more parallelism for the guards that we use
    /// for these circuits.)
    OneHopDirectory,
}

impl Default for GuardUsageKind {
    fn default() -> GuardUsageKind {
        GuardUsageKind::Data
    }
}

/// A set of parameters describing how a single guard should be selected.
///
/// Used as an argument to [`GuardMgr::select_guard`].
#[derive(Clone, Debug, Default, derive_builder::Builder)]
pub struct GuardUsage {
    /// The purpose for which this guard will be used.
    #[builder(default)]
    kind: GuardUsageKind,
    /// An optional restriction on which guard may be used.
    ///
    /// (Eventually, multiple restrictions may be supported.)
    #[builder(default, setter(strip_option))]
    restriction: Option<GuardRestriction>,
}

impl GuardUsageBuilder {
    /// Create a new empty [`GuardUsageBuilder`].
    pub fn new() -> Self {
        Self::default()
    }
}

/// A restriction that applies to a single request for a guard.
///
/// Restrictions differ from filters (see [`GuardFilter`]) in that
/// they apply to single requests, not to our entire set of guards.
/// They're suitable for things like making sure that we don't start
/// and end a circuit at the same relay, or requiring a specific
/// subprotocol version for certain kinds of requests.
#[derive(Clone, Debug)]
#[non_exhaustive]
// XXXX: Should this really be public?
pub enum GuardRestriction {
    /// Don't pick a guard with the provided Ed25519 identity.
    AvoidId(pk::ed25519::Ed25519Identity),
}

#[cfg(test)]
mod test {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn guard_param_defaults() {
        let p1 = GuardParams::default();
        let p2: GuardParams = (&NetParameters::default()).try_into().unwrap();
        assert_eq!(p1, p2);
    }
}