//! Read-only accounting views over the Cargo-pinned Percolator market slab.

use core::mem::{offset_of, size_of};
use percolator::{
    EngineAssetSlotV16Account, Market, MarketGroupV16HeaderAccount, V16ConfigAccount,
};

pub const HEADER_LEN: usize = 16;
pub const WRAPPER_CONFIG_LEN: usize = 448;
pub const ASSET_WRAPPER_LEN: usize = 512;
pub const MARKET_GROUP_OFFSET: usize = HEADER_LEN + WRAPPER_CONFIG_LEN;
pub const INSURANCE_OFFSET: usize =
    MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, insurance);
// Pinned `AssetOracleProfileV16`: four u8s, one u32, five u16s, six bytes
// of padding, then two 32-byte authorities precede the backing authority.
pub const BACKING_AUTHORITY_PROFILE_OFFSET: usize = 88;

const MAGIC: u64 = 0x5045_5243_5631_3600;
const VERSION: u16 = 16;
const KIND_MARKET: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    InvalidHeader,
    InvalidAsset,
    InvalidAccounting,
    Truncated,
}

fn bytes<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N], ReadError> {
    data.get(offset..offset.checked_add(N).ok_or(ReadError::Truncated)?)
        .ok_or(ReadError::Truncated)?
        .try_into()
        .map_err(|_| ReadError::Truncated)
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16, ReadError> {
    Ok(u16::from_le_bytes(bytes(data, offset)?))
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32, ReadError> {
    Ok(u32::from_le_bytes(bytes(data, offset)?))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64, ReadError> {
    Ok(u64::from_le_bytes(bytes(data, offset)?))
}

fn read_u128(data: &[u8], offset: usize) -> Result<u128, ReadError> {
    Ok(u128::from_le_bytes(bytes(data, offset)?))
}

fn validate_market(data: &[u8]) -> Result<(), ReadError> {
    if read_u64(data, 0)? != MAGIC
        || read_u16(data, 8)? != VERSION
        || data.get(10).copied() != Some(KIND_MARKET)
    {
        return Err(ReadError::InvalidHeader);
    }
    Ok(())
}

fn validate_asset(data: &[u8], asset_index: usize) -> Result<(), ReadError> {
    let header_config = MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, config);
    let configured = read_u32(
        data,
        header_config + offset_of!(V16ConfigAccount, max_market_slots),
    )? as usize;
    let capacity = read_u32(
        data,
        MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, asset_slot_capacity),
    )? as usize;
    if asset_index >= configured || asset_index >= capacity {
        return Err(ReadError::InvalidAsset);
    }
    Ok(())
}

fn asset_wrapper_offset(asset_index: usize) -> Result<usize, ReadError> {
    let slot_stride = size_of::<Market<[u8; ASSET_WRAPPER_LEN]>>();
    MARKET_GROUP_OFFSET
        .checked_add(size_of::<MarketGroupV16HeaderAccount>())
        .and_then(|v| {
            asset_index
                .checked_mul(slot_stride)
                .and_then(|n| v.checked_add(n))
        })
        .ok_or(ReadError::Truncated)
}

fn asset_engine_offset(asset_index: usize) -> Result<usize, ReadError> {
    asset_wrapper_offset(asset_index)?
        .checked_add(ASSET_WRAPPER_LEN)
        .ok_or(ReadError::Truncated)
}

/// Returns the backing authority recorded in one pinned-v16 asset profile.
pub fn read_asset_backing_authority(
    data: &[u8],
    asset_index: usize,
) -> Result<[u8; 32], ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    bytes(
        data,
        asset_wrapper_offset(asset_index)? + BACKING_AUTHORITY_PROFILE_OFFSET,
    )
}

/// Returns the insurance attributed to one asset's long and short domains.
///
/// This mirrors Percolator's `market_insurance_remaining_view`: each domain contributes
/// `budget - spent`, and the sum is capped by the market-wide insurance balance. It does
/// not subtract temporary source-credit reservations; Percolator's withdrawal CPI still
/// enforces those, so an active reservation delays an exit instead of crystallizing it as
/// a depositor loss.
pub fn read_asset_insurance_remaining(data: &[u8], asset_index: usize) -> Result<u128, ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;

    let engine = asset_engine_offset(asset_index)?;
    let long_budget = read_u128(
        data,
        engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_budget_long),
    )?;
    let short_budget = read_u128(
        data,
        engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_budget_short),
    )?;
    let long_spent = read_u128(
        data,
        engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_spent_long),
    )?;
    let short_spent = read_u128(
        data,
        engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_spent_short),
    )?;
    let remaining = long_budget
        .checked_sub(long_spent)
        .and_then(|long| {
            short_budget
                .checked_sub(short_spent)
                .and_then(|short| long.checked_add(short))
        })
        .ok_or(ReadError::InvalidAccounting)?;
    Ok(remaining.min(read_u128(data, INSURANCE_OFFSET)?))
}
