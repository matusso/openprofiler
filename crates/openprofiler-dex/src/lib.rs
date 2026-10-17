//! `openprofiler-dex`: Java decompiler for Android DEX/APK and JVM class files, with smali and
//! bytecode views.
//!
//! [`Program`] opens an `.apk` (or `.xapk`/`.apks`/`.aab`) or `.dex` file, or JVM bytecode (a
//! `.jar`/`.war`/`.ear`/`.jmod` or `.class`, converted to the same model by [`jvm`]), indexes its
//! classes and decompiles top-level classes on demand ([`Program::class_code`]), with navigation
//! links, usages, search and decoded resources. The per-method pipeline is [`method::lift`] (bytecode →
//! IR with recovered variables and types), [`structure::structure`] (control flow → statements)
//! and [`java`] (source text); see docs/DESIGN.md §3.13.
//!
//! Debugging: `cargo run -p openprofiler-dex --example decompile -- app.apk [out-dir]` decompiles a
//! whole app or jar (`ONLY=<descriptor substring>` limits it, `--stats` counts fallbacks); with
//! `OPENPROFILER_DEX_DUMP=1` every method body is preceded by its block graph and statement tree
//! as a comment.

pub mod dex;
pub mod insn;
pub mod ir;
pub mod java;
pub mod jvm;
pub mod method;
pub mod program;
mod read;
pub mod res;
pub mod smali;
pub mod structure;
pub mod types;
mod xref;
pub mod zip;

pub use program::{ClassCode, NodeInfo, Program, View};
