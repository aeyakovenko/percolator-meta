//! Read-only accounting views over the Cargo-pinned Percolator market slab.

use core::mem::{offset_of, size_of};
use percolator::{
    BackingBucketV16Account, EngineAssetSlotV16Account, Market, MarketGroupV16HeaderAccount,
    PortfolioAccountV16Account, ProvenanceHeaderV16Account, V16ConfigAccount, BOUND_SCALE,
};

pub const HEADER_LEN: usize = 16;
pub const WRAPPER_CONFIG_LEN: usize = 448;
pub const ASSET_WRAPPER_LEN: usize = 512;
pub const MARKET_GROUP_OFFSET: usize = HEADER_LEN + WRAPPER_CONFIG_LEN;
pub const INSURANCE_OFFSET: usize =
    MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, insurance);
pub const MARKET_AUTHORITY_OFFSET: usize = HEADER_LEN;
// Pinned `WrapperConfigV16::maintenance_fee_per_slot` follows the three
// market-level mint/authority keys.
pub const MAINTENANCE_FEE_PER_SLOT_OFFSET: usize = HEADER_LEN + 96;
// Pinned `WrapperConfigV16::permissionless_market_init_fee` relative to the account.
pub const PERMISSIONLESS_MARKET_INIT_FEE_OFFSET: usize = HEADER_LEN + 112;
// Pinned `WrapperConfigV16` global stale-resolution fields relative to the account.
pub const PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET: usize = HEADER_LEN + 136;
pub const LAST_GOOD_ORACLE_SLOT_OFFSET: usize = HEADER_LEN + 152;
// Pinned `WrapperConfigV16::free_market_slot_count` relative to the account.
// The wrapper config starts after the 16-byte account header; this field follows
// its authority/mint, fee, resolve, insurance-withdraw, and oracle-policy prefix.
pub const FREE_MARKET_SLOT_COUNT_OFFSET: usize = HEADER_LEN + 198;
// Pinned `AssetOracleProfileV16`: four u8s, one u32, five u16s, and six bytes
// of padding precede the three custody authorities.
pub const INSURANCE_AUTHORITY_PROFILE_OFFSET: usize = 24;
pub const INSURANCE_OPERATOR_PROFILE_OFFSET: usize = 56;
pub const BACKING_AUTHORITY_PROFILE_OFFSET: usize = 88;
// The cold admin is the final field after the oracle feed/price/timestamp arrays.
pub const ASSET_ADMIN_PROFILE_OFFSET: usize = 368;

const MAGIC: u64 = 0x5045_5243_5631_3600;
const VERSION: u16 = 16;
const KIND_MARKET: u8 = 1;
const KIND_PORTFOLIO: u8 = 2;
const PORTFOLIO_ACCOUNT_VERSION: u16 = 1;
const PORTFOLIO_LAYOUT_DISCRIMINATOR: u16 = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    InvalidHeader,
    InvalidAsset,
    InvalidAccounting,
    Truncated,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackingDomainBalance {
    pub principal_atoms: u128,
    pub earnings_atoms: u128,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortfolioRewardSnapshot {
    pub market_group: [u8; 32],
    pub portfolio: [u8; 32],
    pub owner: [u8; 32],
    pub residual_crystallized_loss: u128,
    pub residual_spent_principal: u128,
    pub residual_received: u128,
    pub funding_long_paid: u128,
    pub funding_long_received: u128,
    pub funding_short_paid: u128,
    pub funding_short_received: u128,
}

impl PortfolioRewardSnapshot {
    pub fn has_reward_telemetry(&self) -> bool {
        self.residual_crystallized_loss != 0
            || self.residual_spent_principal != 0
            || self.residual_received != 0
            || self.funding_long_paid != 0
            || self.funding_short_paid != 0
    }
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

fn validate_header(data: &[u8], kind: u8) -> Result<(), ReadError> {
    if read_u64(data, 0)? != MAGIC
        || read_u16(data, 8)? != VERSION
        || data.get(10).copied() != Some(kind)
    {
        return Err(ReadError::InvalidHeader);
    }
    Ok(())
}

fn validate_market(data: &[u8]) -> Result<(), ReadError> {
    validate_header(data, KIND_MARKET)
}

/// Reads the immutable portfolio identity and monotonic reward counters from the
/// Cargo-pinned Percolator layout. The account key is part of the authenticated
/// provenance and must match the transaction account carrying these bytes.
pub fn read_portfolio_reward_snapshot(
    data: &[u8],
    portfolio: &[u8; 32],
) -> Result<PortfolioRewardSnapshot, ReadError> {
    validate_header(data, KIND_PORTFOLIO)?;
    let base = HEADER_LEN;
    let provenance = base + offset_of!(PortfolioAccountV16Account, provenance_header);
    let market_group = bytes(
        data,
        provenance + offset_of!(ProvenanceHeaderV16Account, market_group_id),
    )?;
    let recorded_portfolio = bytes(
        data,
        provenance + offset_of!(ProvenanceHeaderV16Account, portfolio_account_id),
    )?;
    let provenance_owner = bytes(
        data,
        provenance + offset_of!(ProvenanceHeaderV16Account, owner),
    )?;
    let account_version = read_u16(
        data,
        provenance + offset_of!(ProvenanceHeaderV16Account, version),
    )?;
    let layout_discriminator = read_u16(
        data,
        provenance + offset_of!(ProvenanceHeaderV16Account, layout_discriminator),
    )?;
    let owner = bytes(data, base + offset_of!(PortfolioAccountV16Account, owner))?;
    if recorded_portfolio != *portfolio
        || provenance_owner != owner
        || market_group == [0u8; 32]
        || owner == [0u8; 32]
        || account_version != PORTFOLIO_ACCOUNT_VERSION
        || layout_discriminator != PORTFOLIO_LAYOUT_DISCRIMINATOR
    {
        return Err(ReadError::InvalidAccounting);
    }
    let counter = |offset| read_u128(data, base + offset);
    Ok(PortfolioRewardSnapshot {
        market_group,
        portfolio: recorded_portfolio,
        owner,
        residual_crystallized_loss: counter(offset_of!(
            PortfolioAccountV16Account,
            residual_crystallized_loss_atoms_total
        ))?,
        residual_spent_principal: counter(offset_of!(
            PortfolioAccountV16Account,
            residual_spent_principal_atoms_total
        ))?,
        residual_received: counter(offset_of!(
            PortfolioAccountV16Account,
            residual_received_atoms_total
        ))?,
        funding_long_paid: counter(offset_of!(
            PortfolioAccountV16Account,
            funding_long_paid_atoms_total
        ))?,
        funding_long_received: counter(offset_of!(
            PortfolioAccountV16Account,
            funding_long_received_atoms_total
        ))?,
        funding_short_paid: counter(offset_of!(
            PortfolioAccountV16Account,
            funding_short_paid_atoms_total
        ))?,
        funding_short_received: counter(offset_of!(
            PortfolioAccountV16Account,
            funding_short_received_atoms_total
        ))?,
    })
}

fn validate_asset(data: &[u8], asset_index: usize) -> Result<(), ReadError> {
    let (configured, capacity) = market_slot_counts(data)?;
    if asset_index >= configured || asset_index >= capacity {
        return Err(ReadError::InvalidAsset);
    }
    Ok(())
}

fn market_slot_counts(data: &[u8]) -> Result<(usize, usize), ReadError> {
    let header_config = MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, config);
    let configured = read_u32(
        data,
        header_config + offset_of!(V16ConfigAccount, max_market_slots),
    )? as usize;
    let capacity = read_u32(
        data,
        MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, asset_slot_capacity),
    )? as usize;
    if configured == 0 || configured > capacity {
        return Err(ReadError::InvalidAccounting);
    }
    Ok((configured, capacity))
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

/// Returns the market-level authority in the pinned wrapper config.
pub fn read_market_authority(data: &[u8]) -> Result<[u8; 32], ReadError> {
    validate_market(data)?;
    bytes(data, MARKET_AUTHORITY_OFFSET)
}

/// Returns the immutable account-level maintenance fee from the pinned wrapper config.
pub fn read_maintenance_fee_per_slot(data: &[u8]) -> Result<u128, ReadError> {
    validate_market(data)?;
    read_u128(data, MAINTENANCE_FEE_PER_SLOT_OFFSET)
}

/// Returns the fee that enables direct, non-marketauth secondary-slot activation.
pub fn read_permissionless_market_init_fee(data: &[u8]) -> Result<u128, ReadError> {
    validate_market(data)?;
    read_u128(data, PERMISSIONLESS_MARKET_INIT_FEE_OFFSET)
}

/// Returns whether Percolator's whole-market stale resolution is permissionless now.
///
/// Percolator authenticates instruction-supplied slots against both the Clock sysvar
/// and its monotonic market slot. Controller value paths must use the same maximum so
/// they cannot race a resolution snapshot using a lagging Clock or market header.
pub fn permissionless_resolution_matured(
    data: &[u8],
    clock_slot: u64,
) -> Result<bool, ReadError> {
    validate_market(data)?;
    let stale_slots = read_u64(data, PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET)?;
    if stale_slots == 0 {
        return Ok(false);
    }
    let market_slot = read_u64(
        data,
        MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, current_slot),
    )?;
    let now_slot = clock_slot.max(market_slot);
    let last_good_oracle_slot = read_u64(data, LAST_GOOD_ORACLE_SLOT_OFFSET)?;
    Ok(now_slot.saturating_sub(last_good_oracle_slot) >= stale_slots)
}

/// Returns true when asset 0 is the only non-retired configured slot.
///
/// Percolator increments `free_market_slot_count` only after a secondary slot is
/// fully retired and canonicalized. This O(1) view lets a lifecycle handoff reject
/// active secondary admins without scanning a dynamically sized market account.
pub fn all_secondary_assets_retired(data: &[u8]) -> Result<bool, ReadError> {
    validate_market(data)?;
    let (configured, _) = market_slot_counts(data)?;
    let free = usize::from(read_u16(data, FREE_MARKET_SLOT_COUNT_OFFSET)?);
    let secondary_slots = configured - 1;
    if free > secondary_slots {
        return Err(ReadError::InvalidAccounting);
    }
    Ok(free == secondary_slots)
}

/// Returns true only after Percolator has resolved the market and every materialized
/// portfolio/capital balance has exited. Value-moving CPIs repeat these checks; this
/// view prevents a fixed authority transition from running before its first CPI.
pub fn market_is_resolved_and_empty(data: &[u8]) -> Result<bool, ReadError> {
    validate_market(data)?;
    let header = MARKET_GROUP_OFFSET;
    let mode = data
        .get(header + offset_of!(MarketGroupV16HeaderAccount, mode))
        .copied()
        .ok_or(ReadError::Truncated)?;
    let c_tot = read_u128(
        data,
        header + offset_of!(MarketGroupV16HeaderAccount, c_tot),
    )?;
    let portfolios = read_u64(
        data,
        header + offset_of!(MarketGroupV16HeaderAccount, materialized_portfolio_count),
    )?;
    Ok(mode == 1 && c_tot == 0 && portfolios == 0)
}

/// Returns the withdrawable principal and provider earnings for both domains of one
/// asset. Principal is stored as `BOUND_SCALE` numerators by the engine; deposits and
/// withdrawal deltas are atom-exact, so a non-integral value is invalid accounting.
pub fn read_asset_backing_balances(
    data: &[u8],
    asset_index: usize,
) -> Result<[BackingDomainBalance; 2], ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    let engine = asset_engine_offset(asset_index)?;
    let read_bucket = |bucket_offset: usize| -> Result<BackingDomainBalance, ReadError> {
        let bucket = engine
            .checked_add(bucket_offset)
            .ok_or(ReadError::Truncated)?;
        let principal_num = read_u128(
            data,
            bucket + offset_of!(BackingBucketV16Account, fresh_unliened_backing_num),
        )?;
        if principal_num % BOUND_SCALE != 0 {
            return Err(ReadError::InvalidAccounting);
        }
        Ok(BackingDomainBalance {
            principal_atoms: principal_num / BOUND_SCALE,
            earnings_atoms: read_u128(
                data,
                bucket + offset_of!(BackingBucketV16Account, utilization_fee_earnings),
            )?,
        })
    };
    Ok([
        read_bucket(offset_of!(EngineAssetSlotV16Account, backing_long))?,
        read_bucket(offset_of!(EngineAssetSlotV16Account, backing_short))?,
    ])
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

/// Returns the insurance withdrawal authority recorded in one pinned-v16 asset profile.
pub fn read_asset_insurance_authority(
    data: &[u8],
    asset_index: usize,
) -> Result<[u8; 32], ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    bytes(
        data,
        asset_wrapper_offset(asset_index)? + INSURANCE_AUTHORITY_PROFILE_OFFSET,
    )
}

/// Returns the live insurance operator recorded in one pinned-v16 asset profile.
pub fn read_asset_insurance_operator(
    data: &[u8],
    asset_index: usize,
) -> Result<[u8; 32], ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    bytes(
        data,
        asset_wrapper_offset(asset_index)? + INSURANCE_OPERATOR_PROFILE_OFFSET,
    )
}

/// Returns the cold-storage admin recorded in one pinned-v16 asset profile.
pub fn read_asset_admin(data: &[u8], asset_index: usize) -> Result<[u8; 32], ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    bytes(
        data,
        asset_wrapper_offset(asset_index)? + ASSET_ADMIN_PROFILE_OFFSET,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn portfolio_with_reward_snapshot(
        market: [u8; 32],
        portfolio: [u8; 32],
        owner: [u8; 32],
    ) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_LEN + size_of::<PortfolioAccountV16Account>()];
        data[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        data[8..10].copy_from_slice(&VERSION.to_le_bytes());
        data[10] = KIND_PORTFOLIO;
        let provenance =
            HEADER_LEN + offset_of!(PortfolioAccountV16Account, provenance_header);
        data[provenance + offset_of!(ProvenanceHeaderV16Account, market_group_id)
            ..provenance + offset_of!(ProvenanceHeaderV16Account, market_group_id) + 32]
            .copy_from_slice(&market);
        data[provenance + offset_of!(ProvenanceHeaderV16Account, portfolio_account_id)
            ..provenance + offset_of!(ProvenanceHeaderV16Account, portfolio_account_id) + 32]
            .copy_from_slice(&portfolio);
        data[provenance + offset_of!(ProvenanceHeaderV16Account, owner)
            ..provenance + offset_of!(ProvenanceHeaderV16Account, owner) + 32]
            .copy_from_slice(&owner);
        data[provenance + offset_of!(ProvenanceHeaderV16Account, version)
            ..provenance + offset_of!(ProvenanceHeaderV16Account, version) + 2]
            .copy_from_slice(&PORTFOLIO_ACCOUNT_VERSION.to_le_bytes());
        data[provenance + offset_of!(ProvenanceHeaderV16Account, layout_discriminator)
            ..provenance + offset_of!(ProvenanceHeaderV16Account, layout_discriminator) + 2]
            .copy_from_slice(&PORTFOLIO_LAYOUT_DISCRIMINATOR.to_le_bytes());
        let owner_offset = HEADER_LEN + offset_of!(PortfolioAccountV16Account, owner);
        data[owner_offset..owner_offset + 32].copy_from_slice(&owner);
        let paid = HEADER_LEN
            + offset_of!(PortfolioAccountV16Account, funding_long_paid_atoms_total);
        data[paid..paid + 16].copy_from_slice(&42u128.to_le_bytes());
        data
    }

    fn market_with_stale_slots(stale_slots: u64, last_good: u64, market_slot: u64) -> Vec<u8> {
        let mut data = vec![0u8; MARKET_GROUP_OFFSET + size_of::<MarketGroupV16HeaderAccount>()];
        data[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        data[8..10].copy_from_slice(&VERSION.to_le_bytes());
        data[10] = KIND_MARKET;
        data[PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET
            ..PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET + 8]
            .copy_from_slice(&stale_slots.to_le_bytes());
        data[LAST_GOOD_ORACLE_SLOT_OFFSET..LAST_GOOD_ORACLE_SLOT_OFFSET + 8]
            .copy_from_slice(&last_good.to_le_bytes());
        let current_slot =
            MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, current_slot);
        data[current_slot..current_slot + 8].copy_from_slice(&market_slot.to_le_bytes());
        data
    }

    #[test]
    fn stale_resolution_uses_the_authenticated_slot_and_exact_boundary() {
        let disabled = market_with_stale_slots(0, 100, 1_000);
        assert!(!permissionless_resolution_matured(&disabled, 1_000).unwrap());

        let by_clock = market_with_stale_slots(50, 100, 149);
        assert!(!permissionless_resolution_matured(&by_clock, 149).unwrap());
        assert!(permissionless_resolution_matured(&by_clock, 150).unwrap());

        let by_market = market_with_stale_slots(50, 100, 150);
        assert!(permissionless_resolution_matured(&by_market, 149).unwrap());
    }

    #[test]
    fn maintenance_fee_uses_the_pinned_wrapper_offset() {
        let mut market = market_with_stale_slots(0, 0, 0);
        market[MAINTENANCE_FEE_PER_SLOT_OFFSET..MAINTENANCE_FEE_PER_SLOT_OFFSET + 16]
            .copy_from_slice(&123u128.to_le_bytes());
        assert_eq!(read_maintenance_fee_per_slot(&market), Ok(123));

        market[10] = KIND_PORTFOLIO;
        assert_eq!(
            read_maintenance_fee_per_slot(&market),
            Err(ReadError::InvalidHeader)
        );
    }

    #[test]
    fn portfolio_reward_snapshot_pins_provenance_and_counter_layout() {
        let market = [1u8; 32];
        let portfolio = [2u8; 32];
        let owner = [3u8; 32];
        let data = portfolio_with_reward_snapshot(market, portfolio, owner);
        let snapshot = read_portfolio_reward_snapshot(&data, &portfolio).unwrap();
        assert_eq!(snapshot.market_group, market);
        assert_eq!(snapshot.portfolio, portfolio);
        assert_eq!(snapshot.owner, owner);
        assert_eq!(snapshot.funding_long_paid, 42);
        assert!(snapshot.has_reward_telemetry());

        let mut received_only = PortfolioRewardSnapshot::default();
        received_only.funding_long_received = 1;
        received_only.funding_short_received = 1;
        assert!(
            !received_only.has_reward_telemetry(),
            "funding receivers do not use either received counter for rewards"
        );

        assert_eq!(
            read_portfolio_reward_snapshot(&data, &[9u8; 32]),
            Err(ReadError::InvalidAccounting)
        );

        let mut split_owner = data;
        let header_owner = HEADER_LEN + offset_of!(PortfolioAccountV16Account, owner);
        split_owner[header_owner] ^= 1;
        assert_eq!(
            read_portfolio_reward_snapshot(&split_owner, &portfolio),
            Err(ReadError::InvalidAccounting)
        );
    }
}
