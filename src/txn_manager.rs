//! Transaction management — now handled by the Store trait.
//!
//! The TxnManager functionality (begin, commit, receipt, sync listener)
//! has been moved to `crate::store::Store`. This module is kept for
//! backward compatibility but contains no code.
//!
//! See `src/store/mod.rs` for the Store trait and `src/store/memory.rs`
//! for the InMemoryStore implementation.
