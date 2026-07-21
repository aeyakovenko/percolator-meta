//! Read-only accounting views over the Cargo-pinned Percolator market slab.

use core::mem::{offset_of, size_of};
use percolator::{
    AssetStateV16Account, BackingBucketV16Account, EngineAssetSlotV16Account,
    InsuranceCreditReservationV16Account, Market, MarketGroupV16HeaderAccount,
    PortfolioAccountV16Account, ProvenanceHeaderV16Account, SourceCreditStateV16Account,
    V16ConfigAccount, BOUND_SCALE,
};

pub const HEADER_LEN: usize = 16;
pub const WRAPPER_CONFIG_LEN: usize = 448;
pub const ASSET_WRAPPER_LEN: usize = 512;
pub const MARKET_GROUP_OFFSET: usize = HEADER_LEN + WRAPPER_CONFIG_LEN;
pub const NEXT_MARKET_ID_OFFSET: usize =
    MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, next_market_id);
pub const INSURANCE_OFFSET: usize =
    MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, insurance);
pub const MARKET_AUTHORITY_OFFSET: usize = HEADER_LEN;
// Pinned `WrapperConfigV16::maintenance_fee_per_slot` follows the three
// market-level mint/authority keys.
pub const MAINTENANCE_FEE_PER_SLOT_OFFSET: usize = HEADER_LEN + 96;
// Pinned `WrapperConfigV16::permissionless_market_init_fee` relative to the account.
pub const PERMISSIONLESS_MARKET_INIT_FEE_OFFSET: usize = HEADER_LEN + 112;
// Pinned `WrapperConfigV16::trade_fee_base_bps` follows the permissionless
// market-init fee.
pub const TRADE_FEE_BASE_BPS_OFFSET: usize =
    PERMISSIONLESS_MARKET_INIT_FEE_OFFSET + size_of::<u128>();
// Pinned `WrapperConfigV16` global stale-resolution fields relative to the account.
pub const PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET: usize = HEADER_LEN + 136;
pub const FORCE_CLOSE_DELAY_SLOTS_OFFSET: usize =
    PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET + size_of::<u64>();
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
    /// Principal immediately withdrawable by the backing authority.
    pub principal_atoms: u128,
    /// Principal currently liened to live market exposure but not yet lost.
    pub valid_liened_principal_atoms: u128,
    /// Principal already consumed by counterparty losses.
    pub consumed_principal_atoms: u128,
    /// Principal attached to an impaired lien.
    pub impaired_principal_atoms: u128,
    pub earnings_atoms: u128,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackingSourceCredit {
    pub positive_claim_bound_num: u128,
    pub exact_positive_claim_num: u128,
    pub fresh_reserved_backing_num: u128,
    pub spent_backing_num: u128,
    pub provider_receivable_num: u128,
    pub valid_liened_backing_num: u128,
    pub impaired_liened_backing_num: u128,
    pub insurance_credit_reserved_num: u128,
    pub valid_liened_insurance_num: u128,
    pub impaired_liened_insurance_num: u128,
}

impl BackingDomainBalance {
    pub fn protected_principal_atoms(self) -> Result<u128, ReadError> {
        self.principal_atoms
            .checked_add(self.valid_liened_principal_atoms)
            .ok_or(ReadError::InvalidAccounting)
    }

    pub fn has_any_state(self) -> bool {
        self.principal_atoms != 0
            || self.valid_liened_principal_atoms != 0
            || self.consumed_principal_atoms != 0
            || self.impaired_principal_atoms != 0
            || self.earnings_atoms != 0
    }

    /// Net loss-bearing principal attributable to one provider ledger.
    ///
    /// The aggregate bucket can temporarily contain trader capital that is
    /// already owed to a source-attributed positive claim. Net that liability
    /// before capping the result by canonical provider principal.
    pub fn provider_protected_principal_atoms(
        self,
        provider_principal_atoms: u128,
        source: BackingSourceCredit,
    ) -> Result<u128, ReadError> {
        let protected = self.protected_principal_atoms()?;
        let protected_num = protected
            .checked_mul(BOUND_SCALE)
            .ok_or(ReadError::InvalidAccounting)?;
        let valid_num = self
            .valid_liened_principal_atoms
            .checked_mul(BOUND_SCALE)
            .ok_or(ReadError::InvalidAccounting)?;
        let consumed_num = self
            .consumed_principal_atoms
            .checked_mul(BOUND_SCALE)
            .ok_or(ReadError::InvalidAccounting)?;
        let impaired_num = self
            .impaired_principal_atoms
            .checked_mul(BOUND_SCALE)
            .ok_or(ReadError::InvalidAccounting)?;
        if source.fresh_reserved_backing_num != protected_num
            || source.valid_liened_backing_num != valid_num
            || source.provider_receivable_num != consumed_num
            || source.impaired_liened_backing_num != impaired_num
            || source.exact_positive_claim_num > source.positive_claim_bound_num
            || source.spent_backing_num < source.provider_receivable_num
            || source.exact_positive_claim_num % BOUND_SCALE != 0
        {
            return Err(ReadError::InvalidAccounting);
        }
        let exact_claim_atoms = source.exact_positive_claim_num / BOUND_SCALE;
        Ok(core::cmp::min(
            protected.saturating_sub(exact_claim_atoms),
            provider_principal_atoms,
        ))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackingDomainLedger {
    pub market_group: [u8; 32],
    pub authority: [u8; 32],
    pub total_principal_atoms: u128,
    pub domain: u16,
}

pub fn backing_domain_ledger_account_len() -> usize {
    percolator_prog::state::backing_domain_ledger_account_len()
}

/// Reads one pinned Percolator provider ledger. A program-owned zeroed PDA is
/// the valid pre-first-deposit state and carries zero provider principal.
pub fn read_backing_domain_ledger(data: &[u8]) -> Result<Option<BackingDomainLedger>, ReadError> {
    if data.len() < backing_domain_ledger_account_len() {
        return Err(ReadError::Truncated);
    }
    if data.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    let ledger = percolator_prog::state::read_backing_domain_ledger(data)
        .map_err(|_| ReadError::InvalidHeader)?;
    Ok(Some(BackingDomainLedger {
        market_group: ledger.market_group,
        authority: ledger.authority,
        total_principal_atoms: ledger.total_principal_atoms,
        domain: ledger.domain,
    }))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InsuranceDomainBalance {
    pub remaining_atoms: u128,
    pub withdrawable_atoms: u128,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InsuranceAssetBalance {
    pub domains: [InsuranceDomainBalance; 2],
    pub remaining_atoms: u128,
    pub withdrawable_atoms: u128,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InsuranceWithdrawalPlan {
    pub gross_withdrawal: u128,
    pub redeposit: [u128; 2],
}

/// The canonical odd-atom split used by Meta insurance principal and floors.
pub fn balanced_insurance_domains(total: u128) -> [u128; 2] {
    let long = total / 2;
    [long, total - long]
}

/// Plan one asset-wide Percolator withdrawal followed by domain top-ups so the
/// net payout is `payout` and the two domains finish at `target_remaining`.
///
/// The pinned wrapper consumes every withdrawable long-domain atom before it
/// reaches the short domain. This planner models that exact order, including
/// per-domain reservation floors and the asset-wide global capacity.
pub fn plan_insurance_withdrawal_to_domains(
    balance: InsuranceAssetBalance,
    payout: u128,
    target_remaining: [u128; 2],
) -> Result<InsuranceWithdrawalPlan, ReadError> {
    let domain_total = balance.domains[0]
        .remaining_atoms
        .checked_add(balance.domains[1].remaining_atoms)
        .ok_or(ReadError::InvalidAccounting)?;
    let target_total = target_remaining[0]
        .checked_add(target_remaining[1])
        .ok_or(ReadError::InvalidAccounting)?;
    if domain_total != balance.remaining_atoms
        || payout > balance.remaining_atoms
        || payout > balance.withdrawable_atoms
        || target_total
            != balance
                .remaining_atoms
                .checked_sub(payout)
                .ok_or(ReadError::InvalidAccounting)?
    {
        return Err(ReadError::InvalidAccounting);
    }

    let long_capacity = core::cmp::min(
        balance.withdrawable_atoms,
        balance.domains[0].withdrawable_atoms,
    );
    let short_capacity = balance
        .withdrawable_atoms
        .checked_sub(long_capacity)
        .ok_or(ReadError::InvalidAccounting)?;
    if short_capacity > balance.domains[1].withdrawable_atoms {
        return Err(ReadError::InvalidAccounting);
    }

    let current_long = balance.domains[0].remaining_atoms;
    let current_short = balance.domains[1].remaining_atoms;
    let long_floor = current_long
        .checked_sub(long_capacity)
        .ok_or(ReadError::InvalidAccounting)?;
    let short_floor = current_short
        .checked_sub(short_capacity)
        .ok_or(ReadError::InvalidAccounting)?;
    if target_remaining[0] < long_floor || target_remaining[1] < short_floor {
        return Err(ReadError::InvalidAccounting);
    }

    let (gross_withdrawal, redeposit) = if target_remaining[1] >= current_short {
        let gross = current_long
            .checked_sub(target_remaining[0])
            .ok_or(ReadError::InvalidAccounting)?;
        if gross > long_capacity {
            return Err(ReadError::InvalidAccounting);
        }
        (
            gross,
            [
                0,
                target_remaining[1]
                    .checked_sub(current_short)
                    .ok_or(ReadError::InvalidAccounting)?,
            ],
        )
    } else {
        let short_debit = current_short
            .checked_sub(target_remaining[1])
            .ok_or(ReadError::InvalidAccounting)?;
        if short_debit > short_capacity {
            return Err(ReadError::InvalidAccounting);
        }
        (
            long_capacity
                .checked_add(short_debit)
                .ok_or(ReadError::InvalidAccounting)?,
            [
                target_remaining[0]
                    .checked_sub(long_floor)
                    .ok_or(ReadError::InvalidAccounting)?,
                0,
            ],
        )
    };

    let gross_long = core::cmp::min(gross_withdrawal, long_capacity);
    let gross_short = gross_withdrawal
        .checked_sub(gross_long)
        .ok_or(ReadError::InvalidAccounting)?;
    let final_long = current_long
        .checked_sub(gross_long)
        .and_then(|v| v.checked_add(redeposit[0]))
        .ok_or(ReadError::InvalidAccounting)?;
    let final_short = current_short
        .checked_sub(gross_short)
        .and_then(|v| v.checked_add(redeposit[1]))
        .ok_or(ReadError::InvalidAccounting)?;
    let net = gross_withdrawal
        .checked_sub(
            redeposit[0]
                .checked_add(redeposit[1])
                .ok_or(ReadError::InvalidAccounting)?,
        )
        .ok_or(ReadError::InvalidAccounting)?;
    if gross_withdrawal > balance.withdrawable_atoms
        || gross_short > short_capacity
        || [final_long, final_short] != target_remaining
        || net != payout
    {
        return Err(ReadError::InvalidAccounting);
    }

    Ok(InsuranceWithdrawalPlan {
        gross_withdrawal,
        redeposit,
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortfolioRewardSnapshot {
    pub market_group: [u8; 32],
    pub portfolio: [u8; 32],
    pub owner: [u8; 32],
    pub portfolio_id: u64,
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
    read_portfolio_reward_snapshot_inner(data, portfolio, false)
}

/// Reads a portfolio snapshot for atomic terminal cleanup, including portfolios
/// created before Percolator assigned monotonic portfolio IDs. Callers must not
/// use this compatibility view to register or identify a live reward position.
pub fn read_portfolio_reward_snapshot_for_cleanup(
    data: &[u8],
    portfolio: &[u8; 32],
) -> Result<PortfolioRewardSnapshot, ReadError> {
    read_portfolio_reward_snapshot_inner(data, portfolio, true)
}

fn read_portfolio_reward_snapshot_inner(
    data: &[u8],
    portfolio: &[u8; 32],
    allow_legacy_zero_id: bool,
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
    let portfolio_id = read_u64(data, percolator_prog::constants::PORTFOLIO_ID_OFF)?;
    if recorded_portfolio != *portfolio
        || provenance_owner != owner
        || market_group == [0u8; 32]
        || owner == [0u8; 32]
        || (portfolio_id == 0 && !allow_legacy_zero_id)
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
        portfolio_id,
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

/// Returns the live base trade fee from the pinned wrapper config.
pub fn read_trade_fee_base_bps(data: &[u8]) -> Result<u64, ReadError> {
    validate_market(data)?;
    read_u64(data, TRADE_FEE_BASE_BPS_OFFSET)
}

/// Returns the hard-stale and force-close deadlines from the pinned wrapper config.
pub fn read_permissionless_resolve_policy(data: &[u8]) -> Result<(u64, u64), ReadError> {
    validate_market(data)?;
    Ok((
        read_u64(data, PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET)?,
        read_u64(data, FORCE_CLOSE_DELAY_SLOTS_OFFSET)?,
    ))
}

/// Returns whether Percolator's whole-market stale resolution is permissionless now.
///
/// Percolator authenticates instruction-supplied slots against both the Clock sysvar
/// and its monotonic market slot. Controller value paths must use the same maximum so
/// they cannot race a resolution snapshot using a lagging Clock or market header.
pub fn permissionless_resolution_matured(data: &[u8], clock_slot: u64) -> Result<bool, ReadError> {
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

/// Returns whether the market is in Percolator's live mode.
pub fn market_is_live(data: &[u8]) -> Result<bool, ReadError> {
    validate_market(data)?;
    let mode = data
        .get(MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, mode))
        .copied()
        .ok_or(ReadError::Truncated)?;
    if mode > 1 {
        return Err(ReadError::InvalidAccounting);
    }
    Ok(mode == 0)
}

/// Returns true when resolving at `resolve_slot` would discard a deterministic
/// price or funding segment from an authenticated mark.
///
/// A newly published mark is value-bearing when its first public crank can move
/// the effective price. Funding remains excluded until that activation crank,
/// matching pinned Percolator's anti-retroactivity rule. Once active, either a
/// price move or nonzero two-sided funding must be accrued before resolution.
pub fn resolve_would_skip_committed_accrual(
    data: &[u8],
    resolve_slot: u64,
) -> Result<bool, ReadError> {
    validate_market(data)?;
    let (configured, _) = market_slot_counts(data)?;
    let field = |base: usize, offset: usize| {
        base.checked_add(offset).ok_or(ReadError::Truncated)
    };
    let config = field(
        MARKET_GROUP_OFFSET,
        offset_of!(MarketGroupV16HeaderAccount, config),
    )?;
    let max_accrual_dt_slots = read_u64(
        data,
        field(
            config,
            offset_of!(V16ConfigAccount, max_accrual_dt_slots),
        )?,
    )?;
    let max_abs_funding_e9_per_slot = read_u64(
        data,
        field(
            config,
            offset_of!(V16ConfigAccount, max_abs_funding_e9_per_slot),
        )?,
    )?;
    let max_price_move_bps_per_slot = read_u64(
        data,
        field(
            config,
            offset_of!(V16ConfigAccount, max_price_move_bps_per_slot),
        )?,
    )?;

    for asset_index in 0..configured {
        let asset = asset_engine_offset(asset_index)?
            .checked_add(offset_of!(EngineAssetSlotV16Account, asset))
            .ok_or(ReadError::Truncated)?;
        let oi_long = read_u128(
            data,
            field(asset, offset_of!(AssetStateV16Account, oi_eff_long_q))?,
        )?;
        let oi_short = read_u128(
            data,
            field(asset, offset_of!(AssetStateV16Account, oi_eff_short_q))?,
        )?;
        if oi_long == 0 && oi_short == 0 {
            continue;
        }

        let slot_last = read_u64(
            data,
            field(asset, offset_of!(AssetStateV16Account, slot_last))?,
        )?;
        if slot_last >= resolve_slot {
            continue;
        }
        let profile = percolator_prog::state::read_asset_oracle_profile(data, asset_index)
            .map_err(|_| ReadError::InvalidAccounting)?;
        if !percolator_prog::oracle_v16::profile_is_price_managed(&profile) {
            continue;
        }
        let pending_mark = profile.mark_ewma_last_slot > slot_last;
        let current = read_u64(
            data,
            field(asset, offset_of!(AssetStateV16Account, effective_price))?,
        )?;
        if profile.mark_ewma_e6 == current {
            continue;
        }
        let dt = core::cmp::min(
            resolve_slot
                .checked_sub(slot_last)
                .ok_or(ReadError::InvalidAccounting)?,
            max_accrual_dt_slots,
        );
        let next = percolator_prog::oracle_v16::effective_price_from_target(
            current,
            profile.mark_ewma_e6,
            max_price_move_bps_per_slot,
            dt,
            true,
        );
        let funding_rate = percolator_prog::policy_v16::premium_funding_rate_e9(
            profile.mark_ewma_e6,
            next,
            max_abs_funding_e9_per_slot,
        )
        .ok_or(ReadError::InvalidAccounting)?;
        let balanced = oi_long != 0 && oi_short != 0;
        if next != current || (!pending_mark && balanced && funding_rate != 0) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Returns whether one pinned-v16 asset still has open position accounting or
/// unresolved loss state. A positive live insurance withdrawal is unsafe while
/// this is true because stale portfolios may not yet have realized their loss.
pub fn asset_has_position_or_loss_state(
    data: &[u8],
    asset_index: usize,
) -> Result<bool, ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    let engine = asset_engine_offset(asset_index)?;
    let asset = engine
        .checked_add(offset_of!(EngineAssetSlotV16Account, asset))
        .ok_or(ReadError::Truncated)?;

    let u128_fields = [
        offset_of!(AssetStateV16Account, oi_eff_long_q),
        offset_of!(AssetStateV16Account, oi_eff_short_q),
        offset_of!(AssetStateV16Account, b_long_num),
        offset_of!(AssetStateV16Account, b_short_num),
        offset_of!(AssetStateV16Account, b_epoch_start_long_num),
        offset_of!(AssetStateV16Account, b_epoch_start_short_num),
        offset_of!(AssetStateV16Account, loss_weight_sum_long),
        offset_of!(AssetStateV16Account, loss_weight_sum_short),
        offset_of!(AssetStateV16Account, social_loss_remainder_long_num),
        offset_of!(AssetStateV16Account, social_loss_remainder_short_num),
        offset_of!(AssetStateV16Account, social_loss_dust_long_num),
        offset_of!(AssetStateV16Account, social_loss_dust_short_num),
        offset_of!(AssetStateV16Account, explicit_unallocated_loss_long),
        offset_of!(AssetStateV16Account, explicit_unallocated_loss_short),
    ];
    for field in u128_fields {
        if read_u128(data, asset.checked_add(field).ok_or(ReadError::Truncated)?)? != 0 {
            return Ok(true);
        }
    }

    let u64_fields = [
        offset_of!(AssetStateV16Account, stored_pos_count_long),
        offset_of!(AssetStateV16Account, stored_pos_count_short),
        offset_of!(AssetStateV16Account, stale_account_count_long),
        offset_of!(AssetStateV16Account, stale_account_count_short),
    ];
    for field in u64_fields {
        if read_u64(data, asset.checked_add(field).ok_or(ReadError::Truncated)?)? != 0 {
            return Ok(true);
        }
    }

    let mode_long = *data
        .get(
            asset
                .checked_add(offset_of!(AssetStateV16Account, mode_long))
                .ok_or(ReadError::Truncated)?,
        )
        .ok_or(ReadError::Truncated)?;
    let mode_short = *data
        .get(
            asset
                .checked_add(offset_of!(AssetStateV16Account, mode_short))
                .ok_or(ReadError::Truncated)?,
        )
        .ok_or(ReadError::Truncated)?;
    let pending_long = read_u64(
        data,
        engine
            .checked_add(offset_of!(
                EngineAssetSlotV16Account,
                pending_domain_loss_barrier_long
            ))
            .ok_or(ReadError::Truncated)?,
    )?;
    let pending_short = read_u64(
        data,
        engine
            .checked_add(offset_of!(
                EngineAssetSlotV16Account,
                pending_domain_loss_barrier_short
            ))
            .ok_or(ReadError::Truncated)?,
    )?;
    Ok(mode_long != 0 || mode_short != 0 || pending_long != 0 || pending_short != 0)
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
        let read_principal = |field_offset: usize| -> Result<u128, ReadError> {
            let numerator = read_u128(data, bucket + field_offset)?;
            if numerator % BOUND_SCALE != 0 {
                return Err(ReadError::InvalidAccounting);
            }
            Ok(numerator / BOUND_SCALE)
        };
        Ok(BackingDomainBalance {
            principal_atoms: read_principal(offset_of!(
                BackingBucketV16Account,
                fresh_unliened_backing_num
            ))?,
            valid_liened_principal_atoms: read_principal(offset_of!(
                BackingBucketV16Account,
                valid_liened_backing_num
            ))?,
            consumed_principal_atoms: read_principal(offset_of!(
                BackingBucketV16Account,
                consumed_liened_backing_num
            ))?,
            impaired_principal_atoms: read_principal(offset_of!(
                BackingBucketV16Account,
                impaired_liened_backing_num
            ))?,
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

/// Returns the paired source-credit accounting for both backing domains.
pub fn read_asset_backing_source_credits(
    data: &[u8],
    asset_index: usize,
) -> Result<[BackingSourceCredit; 2], ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    let engine = asset_engine_offset(asset_index)?;
    let read_source = |source_offset: usize| -> Result<BackingSourceCredit, ReadError> {
        let source = engine
            .checked_add(source_offset)
            .ok_or(ReadError::Truncated)?;
        let field = |offset| read_u128(data, source + offset);
        Ok(BackingSourceCredit {
            positive_claim_bound_num: field(offset_of!(
                SourceCreditStateV16Account,
                positive_claim_bound_num
            ))?,
            exact_positive_claim_num: field(offset_of!(
                SourceCreditStateV16Account,
                exact_positive_claim_num
            ))?,
            fresh_reserved_backing_num: field(offset_of!(
                SourceCreditStateV16Account,
                fresh_reserved_backing_num
            ))?,
            spent_backing_num: field(offset_of!(SourceCreditStateV16Account, spent_backing_num))?,
            provider_receivable_num: field(offset_of!(
                SourceCreditStateV16Account,
                provider_receivable_num
            ))?,
            valid_liened_backing_num: field(offset_of!(
                SourceCreditStateV16Account,
                valid_liened_backing_num
            ))?,
            impaired_liened_backing_num: field(offset_of!(
                SourceCreditStateV16Account,
                impaired_liened_backing_num
            ))?,
            insurance_credit_reserved_num: field(offset_of!(
                SourceCreditStateV16Account,
                insurance_credit_reserved_num
            ))?,
            valid_liened_insurance_num: field(offset_of!(
                SourceCreditStateV16Account,
                valid_liened_insurance_num
            ))?,
            impaired_liened_insurance_num: field(offset_of!(
                SourceCreditStateV16Account,
                impaired_liened_insurance_num
            ))?,
        })
    };
    Ok([
        read_source(offset_of!(EngineAssetSlotV16Account, source_credit_long))?,
        read_source(offset_of!(EngineAssetSlotV16Account, source_credit_short))?,
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

/// Returns the engine-assigned generation identifier for one configured asset slot.
pub fn read_asset_market_id(data: &[u8], asset_index: usize) -> Result<u64, ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;
    read_u64(
        data,
        asset_engine_offset(asset_index)?
            + offset_of!(EngineAssetSlotV16Account, asset)
            + offset_of!(percolator::AssetStateV16Account, market_id),
    )
}

/// Returns the market-wide generation cursor. Percolator advances this monotonic
/// identifier whenever any asset slot is created or restarted.
pub fn read_next_market_id(data: &[u8]) -> Result<u64, ReadError> {
    validate_market(data)?;
    read_u64(data, NEXT_MARKET_ID_OFFSET)
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

/// Returns the insurance attributed to one asset's long and short domains, including
/// the amount Percolator currently permits an asset-wide withdrawal to consume.
///
/// The capacity calculation mirrors `domain_insurance_withdraw_capacity` and
/// `market_insurance_withdraw_capacity_view` in the pinned wrapper/engine. Keeping the
/// reservation floor visible lets a caller atomically preserve domain allocation without
/// attempting to move insurance already committed to a source claim.
pub fn read_asset_insurance_balance(
    data: &[u8],
    asset_index: usize,
) -> Result<InsuranceAssetBalance, ReadError> {
    validate_market(data)?;
    validate_asset(data, asset_index)?;

    let engine = asset_engine_offset(asset_index)?;
    let insurance = read_u128(data, INSURANCE_OFFSET)?;
    let vault = read_u128(
        data,
        MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, vault),
    )?;
    let globally_reserved = read_u128(
        data,
        MARKET_GROUP_OFFSET
            + offset_of!(
                MarketGroupV16HeaderAccount,
                source_insurance_credit_reserved_total_atoms
            ),
    )?;
    let global_available = insurance.saturating_sub(globally_reserved).min(vault);

    let read_domain = |budget_offset: usize,
                       spent_offset: usize,
                       reservation_offset: usize|
     -> Result<InsuranceDomainBalance, ReadError> {
        let budget = read_u128(data, engine + budget_offset)?;
        let spent = read_u128(data, engine + spent_offset)?;
        let remaining = budget
            .checked_sub(spent)
            .ok_or(ReadError::InvalidAccounting)?;
        let reserved_num = read_u128(
            data,
            engine
                + reservation_offset
                + offset_of!(
                    InsuranceCreditReservationV16Account,
                    insurance_credit_reserved_num
                ),
        )?;
        let reserved_atoms = (reserved_num / BOUND_SCALE)
            .checked_add(u128::from(reserved_num % BOUND_SCALE != 0))
            .ok_or(ReadError::InvalidAccounting)?;
        Ok(InsuranceDomainBalance {
            remaining_atoms: remaining,
            withdrawable_atoms: remaining
                .saturating_sub(reserved_atoms)
                .min(global_available),
        })
    };

    let domains = [
        read_domain(
            offset_of!(EngineAssetSlotV16Account, insurance_domain_budget_long),
            offset_of!(EngineAssetSlotV16Account, insurance_domain_spent_long),
            offset_of!(EngineAssetSlotV16Account, insurance_reservation_long),
        )?,
        read_domain(
            offset_of!(EngineAssetSlotV16Account, insurance_domain_budget_short),
            offset_of!(EngineAssetSlotV16Account, insurance_domain_spent_short),
            offset_of!(EngineAssetSlotV16Account, insurance_reservation_short),
        )?,
    ];
    let remaining = domains[0]
        .remaining_atoms
        .checked_add(domains[1].remaining_atoms)
        .ok_or(ReadError::InvalidAccounting)?;
    let withdrawable = domains[0]
        .withdrawable_atoms
        .checked_add(domains[1].withdrawable_atoms)
        .ok_or(ReadError::InvalidAccounting)?
        .min(global_available);
    Ok(InsuranceAssetBalance {
        domains,
        remaining_atoms: remaining.min(insurance),
        withdrawable_atoms: withdrawable,
    })
}

/// Returns the live insurance attributed to one asset's two domains.
pub fn read_asset_insurance_remaining(data: &[u8], asset_index: usize) -> Result<u128, ReadError> {
    Ok(read_asset_insurance_balance(data, asset_index)?.remaining_atoms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_u128_at(data: &mut [u8], offset: usize, value: u128) {
        data[offset..offset + 16].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64_at(data: &mut [u8], offset: usize, value: u64) {
        data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn market_with_insurance_capacity() -> Vec<u8> {
        let engine = asset_engine_offset(0).unwrap();
        let mut data = vec![0u8; engine + size_of::<EngineAssetSlotV16Account>()];
        data[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        data[8..10].copy_from_slice(&VERSION.to_le_bytes());
        data[10] = KIND_MARKET;

        let header = MARKET_GROUP_OFFSET;
        let config = header + offset_of!(MarketGroupV16HeaderAccount, config);
        data[config + offset_of!(V16ConfigAccount, max_market_slots)
            ..config + offset_of!(V16ConfigAccount, max_market_slots) + 4]
            .copy_from_slice(&1u32.to_le_bytes());
        data[header + offset_of!(MarketGroupV16HeaderAccount, asset_slot_capacity)
            ..header + offset_of!(MarketGroupV16HeaderAccount, asset_slot_capacity) + 4]
            .copy_from_slice(&1u32.to_le_bytes());

        write_u128_at(&mut data, INSURANCE_OFFSET, 400);
        write_u128_at(
            &mut data,
            header + offset_of!(MarketGroupV16HeaderAccount, vault),
            350,
        );
        write_u128_at(
            &mut data,
            header
                + offset_of!(
                    MarketGroupV16HeaderAccount,
                    source_insurance_credit_reserved_total_atoms
                ),
            20,
        );
        write_u128_at(
            &mut data,
            engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_budget_long),
            200,
        );
        write_u128_at(
            &mut data,
            engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_spent_long),
            20,
        );
        write_u128_at(
            &mut data,
            engine
                + offset_of!(EngineAssetSlotV16Account, insurance_reservation_long)
                + offset_of!(
                    InsuranceCreditReservationV16Account,
                    insurance_credit_reserved_num
                ),
            10 * BOUND_SCALE + 1,
        );
        write_u128_at(
            &mut data,
            engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_budget_short),
            300,
        );
        write_u128_at(
            &mut data,
            engine + offset_of!(EngineAssetSlotV16Account, insurance_domain_spent_short),
            100,
        );
        write_u128_at(
            &mut data,
            engine
                + offset_of!(EngineAssetSlotV16Account, insurance_reservation_short)
                + offset_of!(
                    InsuranceCreditReservationV16Account,
                    insurance_credit_reserved_num
                ),
            50 * BOUND_SCALE,
        );
        data
    }

    fn portfolio_with_reward_snapshot(
        market: [u8; 32],
        portfolio: [u8; 32],
        owner: [u8; 32],
    ) -> Vec<u8> {
        let mut data = vec![0u8; percolator_prog::constants::PORTFOLIO_ACCOUNT_LEN];
        data[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        data[8..10].copy_from_slice(&VERSION.to_le_bytes());
        data[10] = KIND_PORTFOLIO;
        let provenance = HEADER_LEN + offset_of!(PortfolioAccountV16Account, provenance_header);
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
        let paid =
            HEADER_LEN + offset_of!(PortfolioAccountV16Account, funding_long_paid_atoms_total);
        data[paid..paid + 16].copy_from_slice(&42u128.to_le_bytes());
        data[percolator_prog::constants::PORTFOLIO_ID_OFF
            ..percolator_prog::constants::PORTFOLIO_ID_OFF + 8]
            .copy_from_slice(&7u64.to_le_bytes());
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

    fn market_with_committed_funding_segment() -> Vec<u8> {
        let mut wrapper = percolator_prog::state::WrapperConfigV16::default();
        wrapper.marketauth = [1u8; 32];
        wrapper.collateral_mint = [2u8; 32];
        let mut config = percolator::V16Config::public_user_fund(1, 0, 10);
        config.max_accrual_dt_slots = 1;
        config.max_abs_funding_e9_per_slot = 10_000;
        config.max_price_move_bps_per_slot = 1;
        let mut data = vec![
            0u8;
            percolator_prog::state::market_account_len_for_capacity(1).unwrap()
        ];
        percolator_prog::state::init_market_account_zero_copy(
            &mut data,
            &wrapper,
            config,
            [3u8; 32],
            100,
            1,
        )
        .unwrap();

        let asset = asset_engine_offset(0).unwrap()
            + offset_of!(EngineAssetSlotV16Account, asset);
        write_u64_at(
            &mut data,
            asset + offset_of!(AssetStateV16Account, slot_last),
            2,
        );
        write_u64_at(
            &mut data,
            asset + offset_of!(AssetStateV16Account, effective_price),
            100,
        );
        write_u128_at(
            &mut data,
            asset + offset_of!(AssetStateV16Account, oi_eff_long_q),
            1,
        );
        write_u128_at(
            &mut data,
            asset + offset_of!(AssetStateV16Account, oi_eff_short_q),
            1,
        );
        let mut profile = percolator_prog::state::read_asset_oracle_profile(&data, 0).unwrap();
        profile.oracle_mode = percolator_prog::constants::ORACLE_MODE_AUTH_MARK;
        profile.mark_ewma_e6 = 99;
        profile.mark_ewma_last_slot = 2;
        profile.mark_ewma_halflife_slots = 0;
        profile.mark_min_fee = 0;
        profile.oracle_target_price_e6 = 99;
        percolator_prog::state::write_asset_oracle_profile(&mut data, 0, &profile).unwrap();
        data
    }

    #[test]
    fn stale_resolution_uses_the_authenticated_slot_and_exact_boundary() {
        assert_eq!(
            PERMISSIONLESS_RESOLVE_STALE_SLOTS_OFFSET,
            HEADER_LEN
                + offset_of!(
                    percolator_prog::state::WrapperConfigV16,
                    permissionless_resolve_stale_slots
                )
        );
        assert_eq!(
            FORCE_CLOSE_DELAY_SLOTS_OFFSET,
            HEADER_LEN
                + offset_of!(
                    percolator_prog::state::WrapperConfigV16,
                    force_close_delay_slots
                )
        );
        let mut policy = market_with_stale_slots(50, 100, 149);
        policy[FORCE_CLOSE_DELAY_SLOTS_OFFSET..FORCE_CLOSE_DELAY_SLOTS_OFFSET + 8]
            .copy_from_slice(&20u64.to_le_bytes());
        assert_eq!(read_permissionless_resolve_policy(&policy), Ok((50, 20)));

        let disabled = market_with_stale_slots(0, 100, 1_000);
        assert!(!permissionless_resolution_matured(&disabled, 1_000).unwrap());

        let by_clock = market_with_stale_slots(50, 100, 149);
        assert!(!permissionless_resolution_matured(&by_clock, 149).unwrap());
        assert!(permissionless_resolution_matured(&by_clock, 150).unwrap());

        let by_market = market_with_stale_slots(50, 100, 150);
        assert!(permissionless_resolution_matured(&by_market, 149).unwrap());
    }

    #[test]
    fn resolve_preflight_rejects_value_bearing_authenticated_marks() {
        let mut market = market_with_committed_funding_segment();
        assert_eq!(resolve_would_skip_committed_accrual(&market, 3), Ok(true));

        let asset = asset_engine_offset(0).unwrap()
            + offset_of!(EngineAssetSlotV16Account, asset);
        let long_offset = asset + offset_of!(AssetStateV16Account, oi_eff_long_q);
        let short_offset = asset + offset_of!(AssetStateV16Account, oi_eff_short_q);
        write_u128_at(&mut market, long_offset, 0);
        write_u128_at(&mut market, short_offset, 0);
        assert_eq!(
            resolve_would_skip_committed_accrual(&market, 3),
            Ok(false),
            "an unexposed asset cannot lose position value during resolution"
        );
        write_u128_at(&mut market, long_offset, 1);
        write_u128_at(&mut market, short_offset, 1);

        let mut profile = percolator_prog::state::read_asset_oracle_profile(&market, 0).unwrap();
        profile.mark_ewma_e6 = 100;
        profile.oracle_target_price_e6 = 100;
        percolator_prog::state::write_asset_oracle_profile(&mut market, 0, &profile).unwrap();
        assert_eq!(
            resolve_would_skip_committed_accrual(&market, 3),
            Ok(false),
            "a settled target cannot create a skipped price or funding segment"
        );

        profile.mark_ewma_e6 = 99;
        profile.oracle_target_price_e6 = 99;
        profile.oracle_mode = percolator_prog::constants::ORACLE_MODE_MANUAL;
        percolator_prog::state::write_asset_oracle_profile(&mut market, 0, &profile).unwrap();
        assert_eq!(
            resolve_would_skip_committed_accrual(&market, 3),
            Ok(false),
            "manual-price assets have no authenticated mark to commit"
        );

        profile.oracle_mode = percolator_prog::constants::ORACLE_MODE_AUTH_MARK;
        profile.mark_ewma_last_slot = 3;
        percolator_prog::state::write_asset_oracle_profile(&mut market, 0, &profile).unwrap();
        assert_eq!(
            resolve_would_skip_committed_accrual(&market, 3),
            Ok(false),
            "a pending zero-move mark cannot accrue funding on its activation crank"
        );

        profile.mark_ewma_e6 = 20_000;
        profile.oracle_target_price_e6 = 20_000;
        percolator_prog::state::write_asset_oracle_profile(&mut market, 0, &profile).unwrap();
        write_u64_at(
            &mut market,
            asset + offset_of!(AssetStateV16Account, effective_price),
            10_000,
        );
        assert_eq!(
            resolve_would_skip_committed_accrual(&market, 3),
            Ok(true),
            "resolution cannot erase a pending mark with an executable bounded price step"
        );

        profile.mark_ewma_last_slot = 2;
        profile.mark_ewma_e6 = 99;
        profile.oracle_target_price_e6 = 99;
        percolator_prog::state::write_asset_oracle_profile(&mut market, 0, &profile).unwrap();
        write_u64_at(
            &mut market,
            asset + offset_of!(AssetStateV16Account, effective_price),
            100,
        );
        write_u64_at(
            &mut market,
            asset + offset_of!(AssetStateV16Account, slot_last),
            3,
        );
        assert_eq!(resolve_would_skip_committed_accrual(&market, 3), Ok(false));
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
    fn trade_fee_uses_the_pinned_wrapper_offset() {
        let mut market = market_with_stale_slots(0, 0, 0);
        market[TRADE_FEE_BASE_BPS_OFFSET..TRADE_FEE_BASE_BPS_OFFSET + 8]
            .copy_from_slice(&321u64.to_le_bytes());
        assert_eq!(read_trade_fee_base_bps(&market), Ok(321));

        market[10] = KIND_PORTFOLIO;
        assert_eq!(
            read_trade_fee_base_bps(&market),
            Err(ReadError::InvalidHeader)
        );
    }

    #[test]
    fn asset_market_id_uses_the_pinned_engine_slot_offset() {
        let mut market = market_with_insurance_capacity();
        let offset = asset_engine_offset(0).unwrap()
            + offset_of!(EngineAssetSlotV16Account, asset)
            + offset_of!(percolator::AssetStateV16Account, market_id);
        market[offset..offset + 8].copy_from_slice(&77u64.to_le_bytes());
        assert_eq!(read_asset_market_id(&market, 0), Ok(77));
        assert_eq!(
            read_asset_market_id(&market, 1),
            Err(ReadError::InvalidAsset)
        );
    }

    #[test]
    fn next_market_id_uses_the_pinned_market_header_offset() {
        let mut market = market_with_insurance_capacity();
        market[NEXT_MARKET_ID_OFFSET..NEXT_MARKET_ID_OFFSET + 8]
            .copy_from_slice(&91u64.to_le_bytes());
        assert_eq!(read_next_market_id(&market), Ok(91));

        market[10] = KIND_PORTFOLIO;
        assert_eq!(read_next_market_id(&market), Err(ReadError::InvalidHeader));
    }

    #[test]
    fn insurance_capacity_uses_pinned_domain_reservation_and_global_offsets() {
        let mut market = market_with_insurance_capacity();
        assert_eq!(
            read_asset_insurance_balance(&market, 0),
            Ok(InsuranceAssetBalance {
                domains: [
                    InsuranceDomainBalance {
                        remaining_atoms: 180,
                        withdrawable_atoms: 169,
                    },
                    InsuranceDomainBalance {
                        remaining_atoms: 200,
                        withdrawable_atoms: 150,
                    },
                ],
                remaining_atoms: 380,
                withdrawable_atoms: 319,
            })
        );

        write_u128_at(
            &mut market,
            MARKET_GROUP_OFFSET + offset_of!(MarketGroupV16HeaderAccount, vault),
            250,
        );
        assert_eq!(
            read_asset_insurance_balance(&market, 0)
                .unwrap()
                .withdrawable_atoms,
            250,
            "the asset-wide view applies the pinned global vault cap after summing domains",
        );
    }

    #[test]
    fn insurance_capacity_rejects_spent_above_a_domain_budget() {
        let mut market = market_with_insurance_capacity();
        write_u128_at(
            &mut market,
            asset_engine_offset(0).unwrap()
                + offset_of!(EngineAssetSlotV16Account, insurance_domain_spent_long),
            201,
        );
        assert_eq!(
            read_asset_insurance_balance(&market, 0),
            Err(ReadError::InvalidAccounting)
        );
    }

    #[test]
    fn position_or_loss_gate_covers_every_pinned_asset_local_blocker() {
        let mut market = market_with_insurance_capacity();
        assert_eq!(asset_has_position_or_loss_state(&market, 0), Ok(false));

        let engine = asset_engine_offset(0).unwrap();
        let asset = engine + offset_of!(EngineAssetSlotV16Account, asset);
        let u128_fields = [
            offset_of!(AssetStateV16Account, oi_eff_long_q),
            offset_of!(AssetStateV16Account, oi_eff_short_q),
            offset_of!(AssetStateV16Account, b_long_num),
            offset_of!(AssetStateV16Account, b_short_num),
            offset_of!(AssetStateV16Account, b_epoch_start_long_num),
            offset_of!(AssetStateV16Account, b_epoch_start_short_num),
            offset_of!(AssetStateV16Account, loss_weight_sum_long),
            offset_of!(AssetStateV16Account, loss_weight_sum_short),
            offset_of!(AssetStateV16Account, social_loss_remainder_long_num),
            offset_of!(AssetStateV16Account, social_loss_remainder_short_num),
            offset_of!(AssetStateV16Account, social_loss_dust_long_num),
            offset_of!(AssetStateV16Account, social_loss_dust_short_num),
            offset_of!(AssetStateV16Account, explicit_unallocated_loss_long),
            offset_of!(AssetStateV16Account, explicit_unallocated_loss_short),
        ];
        for field in u128_fields {
            write_u128_at(&mut market, asset + field, 1);
            assert_eq!(asset_has_position_or_loss_state(&market, 0), Ok(true));
            write_u128_at(&mut market, asset + field, 0);
        }

        let u64_fields = [
            offset_of!(AssetStateV16Account, stored_pos_count_long),
            offset_of!(AssetStateV16Account, stored_pos_count_short),
            offset_of!(AssetStateV16Account, stale_account_count_long),
            offset_of!(AssetStateV16Account, stale_account_count_short),
        ];
        for field in u64_fields {
            write_u64_at(&mut market, asset + field, 1);
            assert_eq!(asset_has_position_or_loss_state(&market, 0), Ok(true));
            write_u64_at(&mut market, asset + field, 0);
        }

        for field in [
            offset_of!(AssetStateV16Account, mode_long),
            offset_of!(AssetStateV16Account, mode_short),
        ] {
            market[asset + field] = 1;
            assert_eq!(asset_has_position_or_loss_state(&market, 0), Ok(true));
            market[asset + field] = 0;
        }

        for field in [
            offset_of!(EngineAssetSlotV16Account, pending_domain_loss_barrier_long),
            offset_of!(EngineAssetSlotV16Account, pending_domain_loss_barrier_short),
        ] {
            write_u64_at(&mut market, engine + field, 1);
            assert_eq!(asset_has_position_or_loss_state(&market, 0), Ok(true));
            write_u64_at(&mut market, engine + field, 0);
        }

        assert_eq!(asset_has_position_or_loss_state(&market, 0), Ok(false));
        assert_eq!(
            asset_has_position_or_loss_state(&market, 1),
            Err(ReadError::InvalidAsset)
        );
    }

    fn insurance_balance(
        remaining: [u128; 2],
        withdrawable: [u128; 2],
        total_withdrawable: u128,
    ) -> InsuranceAssetBalance {
        InsuranceAssetBalance {
            domains: [
                InsuranceDomainBalance {
                    remaining_atoms: remaining[0],
                    withdrawable_atoms: withdrawable[0],
                },
                InsuranceDomainBalance {
                    remaining_atoms: remaining[1],
                    withdrawable_atoms: withdrawable[1],
                },
            ],
            remaining_atoms: remaining[0] + remaining[1],
            withdrawable_atoms: total_withdrawable,
        }
    }

    #[test]
    fn withdrawal_plan_preserves_a_balanced_floor_across_long_first_debit() {
        let plan = plan_insurance_withdrawal_to_domains(
            insurance_balance([750, 750], [750, 750], 1_500),
            400,
            [550, 550],
        )
        .unwrap();
        assert_eq!(
            plan,
            InsuranceWithdrawalPlan {
                gross_withdrawal: 950,
                redeposit: [550, 0],
            }
        );
    }

    #[test]
    fn withdrawal_plan_rebalances_either_deficient_domain() {
        assert_eq!(
            plan_insurance_withdrawal_to_domains(
                insurance_balance([350, 850], [350, 850], 1_200),
                80,
                [560, 560],
            )
            .unwrap(),
            InsuranceWithdrawalPlan {
                gross_withdrawal: 640,
                redeposit: [560, 0],
            },
        );
        assert_eq!(
            plan_insurance_withdrawal_to_domains(
                insurance_balance([800, 400], [800, 400], 1_200),
                80,
                [560, 560],
            )
            .unwrap(),
            InsuranceWithdrawalPlan {
                gross_withdrawal: 240,
                redeposit: [0, 160],
            },
        );
    }

    #[test]
    fn withdrawal_plan_repairs_domains_without_a_net_payout() {
        assert_eq!(
            plan_insurance_withdrawal_to_domains(
                insurance_balance([350, 750], [350, 750], 1_100),
                0,
                [550, 550],
            )
            .unwrap(),
            InsuranceWithdrawalPlan {
                gross_withdrawal: 550,
                redeposit: [550, 0],
            },
        );
        assert_eq!(
            plan_insurance_withdrawal_to_domains(
                insurance_balance([550, 550], [550, 550], 1_100),
                0,
                [550, 550],
            )
            .unwrap(),
            InsuranceWithdrawalPlan::default(),
        );
    }

    #[test]
    fn withdrawal_plan_never_crosses_a_domain_reservation_floor() {
        assert_eq!(
            plan_insurance_withdrawal_to_domains(
                insurance_balance([750, 750], [100, 750], 850),
                400,
                [550, 550],
            ),
            Err(ReadError::InvalidAccounting),
        );
    }

    #[test]
    fn withdrawal_plan_matches_exhaustive_small_long_first_sequences() {
        for current_long in 0..=6u128 {
            for current_short in 0..=6u128 {
                let total = current_long + current_short;
                for long_withdrawable in 0..=current_long {
                    for short_withdrawable in 0..=current_short {
                        let sum_capacity = long_withdrawable + short_withdrawable;
                        for total_withdrawable in 0..=sum_capacity {
                            let balance = insurance_balance(
                                [current_long, current_short],
                                [long_withdrawable, short_withdrawable],
                                total_withdrawable,
                            );
                            let long_capacity =
                                core::cmp::min(total_withdrawable, long_withdrawable);
                            let short_capacity = total_withdrawable - long_capacity;
                            for payout in 0..=core::cmp::min(total, total_withdrawable) {
                                let target_total = total - payout;
                                for target_long in 0..=target_total {
                                    let target = [target_long, target_total - target_long];
                                    let mut reachable = false;
                                    if short_capacity <= short_withdrawable {
                                        for gross in 0..=total_withdrawable {
                                            let gross_long = core::cmp::min(gross, long_capacity);
                                            let gross_short = gross - gross_long;
                                            if gross_short > short_capacity
                                                || gross_long > current_long
                                                || gross_short > current_short
                                            {
                                                continue;
                                            }
                                            let after = [
                                                current_long - gross_long,
                                                current_short - gross_short,
                                            ];
                                            if target[0] < after[0] || target[1] < after[1] {
                                                continue;
                                            }
                                            let redeposit =
                                                [target[0] - after[0], target[1] - after[1]];
                                            if redeposit[0]
                                                .checked_add(redeposit[1])
                                                .and_then(|v| gross.checked_sub(v))
                                                == Some(payout)
                                            {
                                                reachable = true;
                                                break;
                                            }
                                        }
                                    }
                                    let planned = plan_insurance_withdrawal_to_domains(
                                        balance, payout, target,
                                    );
                                    assert_eq!(
                                        planned.is_ok(),
                                        reachable,
                                        "balance={balance:?} payout={payout} target={target:?}",
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn provider_protection_nets_source_claims_and_caps_to_canonical_principal() {
        let source_for =
            |balance: BackingDomainBalance, exact_claim_atoms: u128| BackingSourceCredit {
                positive_claim_bound_num: exact_claim_atoms * BOUND_SCALE,
                exact_positive_claim_num: exact_claim_atoms * BOUND_SCALE,
                fresh_reserved_backing_num: balance.protected_principal_atoms().unwrap()
                    * BOUND_SCALE,
                spent_backing_num: balance.consumed_principal_atoms * BOUND_SCALE,
                provider_receivable_num: balance.consumed_principal_atoms * BOUND_SCALE,
                valid_liened_backing_num: balance.valid_liened_principal_atoms * BOUND_SCALE,
                impaired_liened_backing_num: balance.impaired_principal_atoms * BOUND_SCALE,
                ..BackingSourceCredit::default()
            };

        let detached_source_claim = BackingDomainBalance {
            principal_atoms: 1_001_000,
            ..BackingDomainBalance::default()
        };
        assert_eq!(
            detached_source_claim.provider_protected_principal_atoms(
                1_000,
                source_for(detached_source_claim, 1_001_000),
            ),
            Ok(0),
        );

        let recovered = BackingDomainBalance {
            principal_atoms: 1,
            ..BackingDomainBalance::default()
        };
        assert_eq!(
            recovered.provider_protected_principal_atoms(1, source_for(recovered, 0)),
            Ok(1),
        );

        let partially_available = BackingDomainBalance {
            principal_atoms: 1,
            consumed_principal_atoms: 1,
            ..BackingDomainBalance::default()
        };
        assert_eq!(
            partially_available
                .provider_protected_principal_atoms(2, source_for(partially_available, 0)),
            Ok(1),
        );

        let liened = BackingDomainBalance {
            valid_liened_principal_atoms: 1,
            ..BackingDomainBalance::default()
        };
        assert_eq!(
            liened.provider_protected_principal_atoms(1, source_for(liened, 0)),
            Ok(1),
        );

        let mismatched = BackingSourceCredit::default();
        assert_eq!(
            recovered.provider_protected_principal_atoms(1, mismatched),
            Err(ReadError::InvalidAccounting),
        );
    }

    #[test]
    fn backing_provider_ledger_view_accepts_only_zero_or_pinned_state() {
        let mut data = vec![0u8; backing_domain_ledger_account_len()];
        assert_eq!(read_backing_domain_ledger(&data), Ok(None));

        let ledger = percolator_prog::state::BackingDomainLedgerAccountV16 {
            market_group: [1u8; 32],
            authority: [2u8; 32],
            total_principal_atoms: 7,
            domain: 1,
            ..percolator_prog::state::BackingDomainLedgerAccountV16::default()
        };
        percolator_prog::state::init_backing_domain_ledger(&mut data, &ledger).unwrap();
        assert_eq!(
            read_backing_domain_ledger(&data),
            Ok(Some(BackingDomainLedger {
                market_group: [1u8; 32],
                authority: [2u8; 32],
                total_principal_atoms: 7,
                domain: 1,
            })),
        );

        data[0] ^= 1;
        assert_eq!(
            read_backing_domain_ledger(&data),
            Err(ReadError::InvalidHeader),
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
        assert_eq!(snapshot.portfolio_id, 7);
        assert_eq!(snapshot.funding_long_paid, 42);
        assert!(snapshot.has_reward_telemetry());

        let mut missing_id = data.clone();
        missing_id[percolator_prog::constants::PORTFOLIO_ID_OFF
            ..percolator_prog::constants::PORTFOLIO_ID_OFF + 8]
            .fill(0);
        assert_eq!(
            read_portfolio_reward_snapshot(&missing_id, &portfolio),
            Err(ReadError::InvalidAccounting),
            "reward telemetry requires a nonzero program-assigned incarnation ID"
        );
        let legacy =
            read_portfolio_reward_snapshot_for_cleanup(&missing_id, &portfolio).unwrap();
        assert_eq!(legacy.portfolio_id, 0);
        assert_eq!(legacy.market_group, market);
        assert_eq!(legacy.owner, owner);
        assert_eq!(legacy.funding_long_paid, 42);

        let mut invalid_legacy = missing_id.clone();
        let provenance_owner = HEADER_LEN
            + offset_of!(PortfolioAccountV16Account, provenance_header)
            + offset_of!(ProvenanceHeaderV16Account, owner);
        invalid_legacy[provenance_owner] ^= 1;
        assert_eq!(
            read_portfolio_reward_snapshot_for_cleanup(&invalid_legacy, &portfolio),
            Err(ReadError::InvalidAccounting),
            "the cleanup exception does not weaken provenance validation"
        );

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
