//! # lc-time — cluster time
//!
//! Frame-accurate playback across machines needs every Runner to agree on
//! *what time it is* to well under one frame (16.7 ms at 60 fps). This crate
//! provides that shared time — **cluster time** — and the transport timeline
//! evaluated against it.
//!
//! ## How it works
//!
//! * The **Director's monotonic clock** defines cluster time ([`MasterClock`]).
//!   Monotonic, not wall-clock: an NTP step, a DST change or an operator
//!   fixing the date can never make a show jump.
//! * Each Runner measures the offset between its own monotonic clock and the
//!   Director's with a four-timestamp exchange over UDP ([`wire`], [`net`]) —
//!   the same measurement NTP makes (RFC 5905 §8).
//! * [`ClockEstimator`] turns those noisy measurements into a smooth,
//!   monotonic local→cluster mapping (offset **and** drift), and
//!   [`ClusterClock`] is the thread-safe handle the rest of a Runner reads.
//! * [`TransportTimeline`] holds the transport states published by the
//!   Director and answers "what show time is it at cluster time *t*?".
//!
//! The estimator and the timeline are pure (no sockets, no real clock), so
//! they are tested against simulated networks with known drift and jitter.
//!
//! ## What this does *not* do
//!
//! It makes all Runners agree on the frame *number*. It cannot align the
//! *phase* of each GPU's vertical blank; unlocked displays may be up to one
//! refresh period apart. That needs sync hardware (genlock / framelock).

pub mod clock;
pub mod estimator;
pub mod net;
pub mod timeline;
pub mod wire;

pub use clock::{ClockReading, ClusterClock, LocalClock, MasterClock, MonotonicClock, SkewedClock};
pub use estimator::{ClockEstimator, EstimatorConfig};
pub use timeline::{next_state, TransportChange, TransportTimeline};
