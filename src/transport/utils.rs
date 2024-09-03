use std::sync::Arc;

use anyhow::Result;
use quick_cache::sync::Cache as QuickCache;
use tokio::sync::Mutex;

use nekoton_utils::*;
use ton_block::Deserializable;

use super::models::RawContractState;
use super::Transport;
use crate::core::models::NetworkCapabilities;

#[allow(unused)]
pub struct AccountsCache {
    accounts: QuickCache<ton_block::MsgAddressInt, Arc<RawContractState>>,
}

impl Default for AccountsCache {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl AccountsCache {
    pub fn new() -> Self {
        const DEFAULT_ACCOUNTS_CAPACITY: usize = 100;

        Self::with_capacity(DEFAULT_ACCOUNTS_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            accounts: QuickCache::new(capacity),
        }
    }

    #[allow(unused)]
    pub fn get_account_state(
        &self,
        address: &ton_block::MsgAddressInt,
    ) -> Option<Arc<RawContractState>> {
        self.accounts.get(address)
    }

    #[allow(unused)]
    pub fn update_account_state(
        &self,
        address: &ton_block::MsgAddressInt,
        account: &RawContractState,
    ) {
        self.accounts
            .insert(address.clone(), Arc::new(account.clone()))
    }
}

pub struct ConfigCache {
    use_default_config: bool,
    state: Mutex<Option<ConfigCacheState>>,
}

impl ConfigCache {
    pub fn new(use_default_config: bool) -> Self {
        Self {
            use_default_config,
            state: Mutex::new(if use_default_config {
                Some(ConfigCacheState {
                    capabilities: NetworkCapabilities {
                        global_id: 0,
                        raw: 0,
                    },
                    config: ton_executor::BlockchainConfig::default(),
                    last_key_block_seqno: 0,
                    phase: ConfigCachePhase::WainingNextValidatorsSet { deadline: u32::MAX },
                })
            } else {
                None
            }),
        }
    }

    pub async fn get_blockchain_config(
        &self,
        transport: &dyn Transport,
        clock: &dyn Clock,
        force: bool,
    ) -> Result<(NetworkCapabilities, ton_executor::BlockchainConfig)> {
        let mut cache = self.state.lock().await;

        let now = clock.now_sec_u64() as u32;

        Ok(match &*cache {
            None => {
                let (capabilities, config, key_block_seqno) = fetch_config(transport).await?;
                let phase = compute_next_phase(now, &config, None, key_block_seqno)?;
                *cache = Some(ConfigCacheState {
                    capabilities,
                    config: config.clone(),
                    last_key_block_seqno: key_block_seqno,
                    phase,
                });
                (capabilities, config)
            }
            Some(a) if force && !self.use_default_config || cache_expired(now, a.phase) => {
                let (capabilities, config, key_block_seqno) = fetch_config(transport).await?;
                let phase = compute_next_phase(
                    now,
                    &config,
                    Some(a.last_key_block_seqno),
                    key_block_seqno,
                )?;
                *cache = Some(ConfigCacheState {
                    capabilities,
                    config: config.clone(),
                    last_key_block_seqno: key_block_seqno,
                    phase,
                });
                (capabilities, config)
            }
            Some(a) => (a.capabilities, a.config.clone()),
        })
    }
}

async fn fetch_config(
    transport: &dyn Transport,
) -> Result<(NetworkCapabilities, ton_executor::BlockchainConfig, u32)> {
    let block = transport.get_latest_key_block().await?;
    parse_block_config(&block)
}

fn parse_block_config(
    block: &[u8],
) -> Result<(NetworkCapabilities, ton_executor::BlockchainConfig, u32)> {
    let block = <ever_block::Block as ever_block::Deserializable>::construct_from_bytes(block)?;
    let info = block
        .read_info()
        .map_err(|err| QueryConfigError::InvalidBlock(err.to_string()))?;

    let extra = block
        .read_extra()
        .map_err(|err| QueryConfigError::InvalidBlock(err.to_string()))?;

    let master = extra
        .read_custom()
        .map_err(|err| QueryConfigError::InvalidBlock(err.to_string()))?
        .ok_or_else(|| QueryConfigError::InvalidBlock("No master block extra".to_string()))?;

    let config = master.config().ok_or_else(|| {
        QueryConfigError::InvalidBlock("No config in master block extra".to_string())
    })?;

    let capabilities = NetworkCapabilities {
        global_id: block.global_id,
        raw: config.capabilities(),
    };

    let bytes = ever_block::Serializable::write_to_bytes(config)?;
    let config = ton_block::ConfigParams::construct_from_bytes(&bytes)
        .map_err(|err| QueryConfigError::InvalidBlock(err.to_string()))?;

    let config = ton_executor::BlockchainConfig::with_config(config, block.global_id)
        .map_err(|err| QueryConfigError::InvalidConfig(err.to_string()))?;

    Ok((capabilities, config, info.seq_no()))
}

fn compute_next_phase(
    now: u32,
    config: &ton_executor::BlockchainConfig,
    last_key_block_seqno: Option<u32>,
    fetched_key_block_seqno: u32,
) -> Result<ConfigCachePhase> {
    if matches!(last_key_block_seqno, Some(seqno) if fetched_key_block_seqno == seqno) {
        return Ok(ConfigCachePhase::WaitingKeyBlock);
    }

    let elector_params = config.raw_config().elector_params()?;
    let current_vset = config.raw_config().validator_set()?;

    let elections_end = current_vset.utime_until() - elector_params.elections_end_before;
    if now < elections_end {
        Ok(ConfigCachePhase::WaitingElectionsEnd {
            deadline: elections_end,
        })
    } else {
        Ok(ConfigCachePhase::WainingNextValidatorsSet {
            deadline: current_vset.utime_until(),
        })
    }
}

fn cache_expired(now: u32, phase: ConfigCachePhase) -> bool {
    match phase {
        ConfigCachePhase::WaitingKeyBlock => true,
        ConfigCachePhase::WaitingElectionsEnd { deadline }
        | ConfigCachePhase::WainingNextValidatorsSet { deadline } => now > deadline,
    }
}

struct ConfigCacheState {
    capabilities: NetworkCapabilities,
    config: ton_executor::BlockchainConfig,
    last_key_block_seqno: u32,
    phase: ConfigCachePhase,
}

#[derive(Copy, Clone)]
enum ConfigCachePhase {
    WaitingKeyBlock,
    WaitingElectionsEnd { deadline: u32 },
    WainingNextValidatorsSet { deadline: u32 },
}

#[derive(thiserror::Error, Debug)]
enum QueryConfigError {
    #[error("Invalid block {0}")]
    InvalidBlock(String),
    #[error("Invalid config {0}")]
    InvalidConfig(String),
}

#[cfg(test)]
mod tests {
    use ton_types::HashmapType;

    use super::*;

    #[test]
    fn test_parse_key_block() {
        let block = "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";
        let block = base64::decode(block).unwrap();
        let (info, config, mc_seq_no) = parse_block_config(&block).unwrap();
        assert_eq!(info.global_id, 777);
        assert_eq!(info.raw, 0x1010000242e, "raw {:x}", info.raw);
        let config_root = config.raw_config().config_params.data().unwrap();
        let config_hash = config_root.repr_hash().to_hex_string();
        let ethalon_hash = "43b150139133e64b4d68c9ee5d46297b36cafeb85329436c8345552fa6eb939d";
        assert_eq!(config_hash, ethalon_hash);
        assert_eq!(mc_seq_no, 188130);
    }
}
