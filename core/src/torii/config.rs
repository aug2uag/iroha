//! Configuration as well as the default values for the URLs used for the main endpoints: `p2p`, `telemetry`, but not `api`.
use iroha_config::derive::Configurable;
use iroha_data_model::{transaction::TransactionLimits, uri::DEFAULT_API_URL};
use serde::{Deserialize, Serialize};

/// Default socket for p2p communication
pub const DEFAULT_TORII_P2P_ADDR: &str = "127.0.0.1:1337";
/// Default socket for reporting internal status and metrics
pub const DEFAULT_TORII_TELEMETRY_URL: &str = "127.0.0.1:8180";
/// Default maximum size of single transaction
pub const DEFAULT_TORII_MAX_TRANSACTION_SIZE: usize = 2_usize.pow(15);
/// Default maximum instruction number
pub const DEFAULT_TORII_MAX_INSTRUCTION_NUMBER: u64 = 2_u64.pow(12);
/// Default maximum wasm size
pub const DEFAULT_TORII_MAX_WASM_SIZE: u64 = 2_u64.pow(20); // 1MiB
/// Default upper bound on `content-length` specified in the HTTP request header
pub const DEFAULT_TORII_MAX_CONTENT_LENGTH: usize = 2_usize.pow(12) * 4000;

/// Structure storing the torii-specific elements of configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Configurable)]
#[serde(rename_all = "UPPERCASE")]
#[serde(default)]
#[config(env_prefix = "TORII_")]
pub struct ToriiConfiguration {
    /// Torii URL for p2p communication for consensus and block synchronization purposes.
    pub p2p_addr: String,
    /// Torii URL for client API.
    pub api_url: String,
    /// Torii URL for reporting internal status and metrics for administration.
    pub telemetry_url: String,
    /// Maximum number of bytes in raw transaction. Used to prevent from DOS attacks.
    pub max_transaction_size: usize,
    /// Maximum number of bytes in raw message. Used to prevent from DOS attacks.
    pub max_content_len: usize,
    /// Limits to which transactions must adhere
    pub transaction_limits: TransactionLimits,
}

impl Default for ToriiConfiguration {
    fn default() -> Self {
        Self {
            p2p_addr: DEFAULT_TORII_P2P_ADDR.to_owned(),
            api_url: DEFAULT_API_URL.to_owned(),
            telemetry_url: DEFAULT_TORII_TELEMETRY_URL.to_owned(),
            max_transaction_size: DEFAULT_TORII_MAX_TRANSACTION_SIZE,
            max_content_len: DEFAULT_TORII_MAX_CONTENT_LENGTH,
            transaction_limits: TransactionLimits {
                max_instruction_number: DEFAULT_TORII_MAX_INSTRUCTION_NUMBER,
                max_wasm_size_bytes: DEFAULT_TORII_MAX_WASM_SIZE,
            },
        }
    }
}