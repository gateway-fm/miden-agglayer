pub mod claim;
pub mod logging;
mod miden_client;

pub const COMPONENT: &str = "miden-agglayer";

pub use miden_client::MidenClient;
