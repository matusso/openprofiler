//! Ember's core data model.
//!
//! A [`Profile`] is the source-independent representation of everything Ember knows about a
//! profiled JVM over a time window: interned symbols, a global stack trie, columnar sample sets
//! (CPU, allocation, monitor, …), thread metadata and state history, telemetry series, GC events
//! and bookmarks. JFR imports, `.ember` snapshots and live agent sessions all produce a `Profile`,
//! and every derived view (call trees, hot spots, flame graphs, diffs) is computed from it by the
//! functions in [`views`].

pub mod lttb;
pub mod profile;
pub mod stacks;
pub mod symbols;
pub mod views;

pub use profile::*;
pub use stacks::{StackId, StackTrie};
pub use symbols::{Frame, FrameId, FrameKind, Method, MethodId, StrId, Symbols};
