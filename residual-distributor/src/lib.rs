//! Deterministic, points-based COIN reward epochs.
//!
//! Fixed mode allocates the immutable genesis supply; vault-balance mode allocates COIN
//! accumulated from later TWAP buybacks. Both modes reuse the same register, counter-delta,
//! freeze, and self-claim implementation. Percolator and subledger accounts are read-only;
//! the only token CPI moves the configured COIN mint from a canonical epoch vault to a
//! stake's bound recipient.
//!
//! ## Points source — percolator counters via snapshot-delta (zero ledgers in percolator)
//!
//! Percolator keeps monotonic per-backer scalars and NO ledger. A backer registers a
//! START snapshot here; CRYSTALLIZE reads the live counter and credits
//! `net_delta = counter - residual_snap`.
//! The original Percolator portfolio cohorts remain LP/trader residual cohorts:
//! LP snapshots `residual_received_atoms_total`, and trader snapshots
//! `residual_crystallized_loss_atoms_total - residual_spent_principal_atoms_total`; these residual
//! cohorts are weighted by `floor(log2(now - start_slot))`.
//! An optional funding-payer cohort can be configured with its own bps. It snapshots
//! `funding_long_paid_atoms_total + funding_short_paid_atoms_total`, so one portfolio earns for funding it
//! paid as either side, with no age multiplier. These are monotonic account-level counters maintained by Percolator when
//! funding is settled, so already-settled funding is not lost when `leg.f_snap` advances. The claim-fee
//! `fee_support_bps` is retained in the vault on every portfolio-flow claim.
//!
//! ## Distribution = self-service deterministic claim (the seal path was RETIRED)
//! Each backer's COIN share is paid DIRECTLY from this program's own `vault` by IX_CLAIM:
//! lifecycle is register -> crystallize -> FREEZE (one-shot snapshot of the cohort denominators)
//! -> CLAIM `floor(cohort_supply * points_i / frozen_total_points)`, signed by the claimant. There
//! is no cranker-built distribution proposal and NO `distribution::seal_winner` CPI: the legacy
//! cranker IX_SEAL (tag 3) is RETIRED (see the `tag 3 ... RETIRED` note below). Determinism is
//! enforced by points_to_amount + the frozen denominators; nothing is trusted.
//!
//! VESTIGIAL: `init` still accepts + canonical-binds `distribution_program`/`distribution_config`
//! (the HC init-squat guard for the old seal target) and stores them in Config, but NOTHING reads
//! them post-init now that IX_SEAL is gone. They are retained for serialized-layout stability (the
//! offset canary syncs distribution_program to distribution_program::id()); do NOT reintroduce a
//! seal CPI against them without re-reviewing the self-service claim path that replaced it.

#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;
#[allow(unused_imports)]
use alloc::{format, vec::Vec}; // required by entrypoint!/msg! in SBF builds

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    declare_id,
    entrypoint::ProgramResult,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

declare_id!("Res1dua1Distr1butor111111111111111111111111");

const BPS_DENOMINATOR: u64 = 10_000;
pub const DEFAULT_FEE_SUPPORT_BPS: u16 = 80;

// The ONE deployed distribution program this decider CPIs (finding HK). Pinning it closes the
// HC-residual init-squat flavor: HC binds distribution_config to the canonical PDA *under the passed
// distribution_program*, but a front-runner could pass a FAKE program (deriving the canonical config
// under it) so seal would CPI the fake program and the real COIN-holding distribution would never be
// sealed -> DOS. Synced to distribution_program::id() by tests/offsets.rs.
pub const DISTRIBUTION_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("D1str1but1on11111111111111111111111111111111");
const DISTRIBUTION_CLAIM_WINDOW_SLOTS: u64 = 1_000_000;

const CONFIG_DISC: [u8; 8] = *b"RDCONFG1";
const STAKE_DISC: [u8; 8] = *b"RDSTAKE1";
const PORTFOLIO_ARCHIVE_DISC: [u8; 8] = *b"RDARCH01";
const PORTFOLIO_ARCHIVE_SEED: &[u8] = b"rd_portfolio_archive";
const MARKET_CONTROLLER_SEED: &[u8] = b"market-controller";
const MARKET_CONTROLLER_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("3ueoyr1JepT2DvPxh8LrhdJZ6YsL2sT9Sm7y3TfNyfi9");
// Up to this many ADDITIONAL allow-listed markets beyond the primary `market_group` (finding IL+): the
// portfolio-flow cohorts read percolator portfolio counters that an attacker can manufacture if they control the
// market's oracle, so a portfolio is only countable if its market is on this orchestrator-vetted allow-list.
// The creator stands up N trusted-Pyth markets while holding the market-auth key locally, vets them, then
// transfers that key to the PDA that rotates it to the DAO — so the allow-listed markets cannot later be
// repointed at an attacker oracle. See DESIGN.md "Market allow-list".
// Ten total allow-listed markets: market_group plus nine extras.
const MAX_EXTRA_MARKETS: usize = 9;
// Reward-epoch init carries a full (market, insurance pool, backing pool) tuple atomically. Six
// tuples fit one DAO-member-signed Squads transaction under Solana's packet-size limit.
const MAX_REWARD_EPOCH_MARKETS: usize = 6;
const CONFIG_FUNDING_TAIL_OFF: usize = 466 + 1 + MAX_EXTRA_MARKETS * 32;
const CONFIG_EPOCH_TAIL_OFF: usize = CONFIG_FUNDING_TAIL_OFF + 68;
const PRE_EPOCH_CONFIG_SIZE: usize = CONFIG_EPOCH_TAIL_OFF;
const CONFIG_EXTRA_INSURANCE_POOLS_OFF: usize = CONFIG_EPOCH_TAIL_OFF + 50;
const CONFIG_EXTRA_BACKING_POOLS_OFF: usize =
    CONFIG_EXTRA_INSURANCE_POOLS_OFF + MAX_EXTRA_MARKETS * 32;
const CONFIG_SIZE: usize = CONFIG_EXTRA_BACKING_POOLS_OFF + MAX_EXTRA_MARKETS * 32;
const STAKE_SIZE: usize = 211; // +1 claimed flag (self-service)
const PORTFOLIO_ARCHIVE_SIZE: usize = 264;
                               // Capital + portfolio-flow cohorts. Insurance/backing reward base-unit principal;
                               // LP/trader reward residual counters; optional funding-payer rewards the sum of
                               // Percolator funding-paid counters. See tests/offsets.rs.
const COHORT_INSURANCE: u8 = 0;
const COHORT_BACKING: u8 = 1;
const COHORT_LP: u8 = 2;
const COHORT_TRADER: u8 = 3;
const COHORT_FUNDING_PAYER: u8 = 4;

const SUB_POSITION_DISC: [u8; 8] = *b"SUBPOS01";
#[cfg(test)]
const PERC_MAGIC: u64 = 0x5045_5243_5631_3600;
#[cfg(test)]
const PERC_VERSION: u16 = 16;
#[cfg(test)]
const PERC_KIND_PORTFOLIO: u8 = 2;

const IX_INIT: u8 = 0;
const IX_REGISTER_START: u8 = 1;
const IX_CRYSTALLIZE: u8 = 2;
// tag 3 (legacy cranker IX_SEAL) RETIRED — superseded by the self-service freeze+claim path below.
// Self-service path (replacing the cranker seal). After emission_end, IX_FREEZE snapshots the
// cohort denominators and closes register/crystallize; backers then finalize/claim their own share.
const IX_FREEZE: u8 = 4;
const IX_CLAIM: u8 = 5;
const IX_INIT_REWARD_EPOCH: u8 = 6;
const IX_ARCHIVE_PORTFOLIO: u8 = 7;

const CONFIG_KIND_LEGACY: u8 = 0;
const CONFIG_KIND_REWARD_EPOCH: u8 = 1;
const REWARD_SUPPLY_FIXED: u8 = 0;
const REWARD_SUPPLY_VAULT_BALANCE: u8 = 1;

// ===========================================================================
// Deterministic, gaming-resistant point math  (pure — unit-tested below)
// ===========================================================================

/// Deterministic exact pro-rata split; floor rounding never over-allocates the fixed pool.
///
/// `total_supply * points_i` is a 192-bit intermediate. Capital points can exceed the u128
/// multiplication threshold with entirely legal u64 principal and tenure, so saturating that product
/// would underpay claims and permanently lock COIN. The overflow path below performs binary long
/// division as 64 quotient/remainder steps. It never materializes the wide product and has no new
/// bigint dependency. The caller maintains `points_i <= total_points`; invalid standalone inputs return 0.
pub fn points_to_amount(total_supply: u64, points_i: u128, total_points: u128) -> u64 {
    if total_points == 0 || points_i > total_points {
        return 0;
    }
    if let Some(product) = (total_supply as u128).checked_mul(points_i) {
        return (product / total_points) as u64;
    }

    let mut quotient = 0u128;
    let mut remainder = 0u128;
    for bit in (0..64).rev() {
        quotient *= 2;

        // Double the remainder modulo total_points without overflowing u128.
        let complement = total_points - remainder;
        if remainder >= complement {
            remainder -= complement; // 2 * remainder - total_points
            quotient += 1;
        } else {
            remainder += remainder;
        }

        if (total_supply >> bit) & 1 != 0 {
            // Add points_i modulo total_points, again using the complement to avoid overflow.
            let complement = total_points - points_i;
            if remainder >= complement {
                remainder -= complement;
                quotient += 1;
            } else {
                remainder += points_i;
            }
        }
    }
    quotient as u64
}

fn replace_cohort_points(total: &mut u128, old: u128, new: u128) -> ProgramResult {
    *total = total
        .checked_sub(old)
        .and_then(|remaining| remaining.checked_add(new))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    Ok(())
}

/// `points` is constructed as `tenure_multiplier * frozen_net`; cancel the common factor before
/// applying the live cap so the intermediate cannot overflow u128.
fn cap_residual_points(
    points: u128,
    frozen_net: u128,
    cap_net: u128,
) -> Result<u128, ProgramError> {
    if frozen_net == 0 || cap_net >= frozen_net {
        return Ok(points);
    }
    if points % frozen_net != 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    (points / frozen_net)
        .checked_mul(cap_net)
        .ok_or(ProgramError::ArithmeticOverflow)
}

fn trader_live_cap(
    prior_net: u128,
    spent_at_snapshot: u128,
    current_net: u128,
    current_spent: u128,
) -> u128 {
    let spent_since_snapshot = current_spent.saturating_sub(spent_at_snapshot);
    core::cmp::min(prior_net.saturating_sub(spent_since_snapshot), current_net)
}

// percolator account header length (KIND/version/etc.) — all percolator account reads below are at
// PERC_HEADER_LEN + within-struct offset, PINNED against the real structs by tests/offsets.rs
// (offset_of! + HEADER_LEN), finding-T discipline.
pub const PERC_HEADER_LEN: usize = 16;

fn read_u128(data: &[u8], off: usize) -> Result<u128, ProgramError> {
    let b = data
        .get(off..off + 16)
        .ok_or(ProgramError::AccountDataTooSmall)?;
    Ok(u128::from_le_bytes(b.try_into().unwrap()))
}

#[cfg(test)]
fn read_pubkey(data: &[u8], off: usize) -> Result<Pubkey, ProgramError> {
    let b = data
        .get(off..off + 32)
        .ok_or(ProgramError::AccountDataTooSmall)?;
    Ok(Pubkey::new_from_array(b.try_into().unwrap()))
}

// ===========================================================================
// Insurance cohort (the SOFT VETO half) — points read LIVE from the subledger
// position, so an owner exit (principal -> 0, withdrawn) AUTO-FORFEITS the COIN
// share. A permissionless finalized-genesis return preserves only the principal
// still at risk when the cranker returned it.
// ===========================================================================
// subledger Position offsets (stable across the share-model change — appended
// fields only): principal u64@72, withdrawn u8@88, start_slot u64@89.
// Subledger Position offsets. PINNED against the subledger's exported POS_* consts by
// tests/offsets.rs (finding HF: a wrong owner offset here slipped past mocked tests).
pub const SUB_POS_POOL: usize = 8; // Position.pool @ 8 (real layout: disc@0, pool@8..40, owner@40..72).
pub const SUB_POS_OWNER: usize = 40; // Position.owner @ 40. The depositor owed this position's COIN.
pub const SUB_POS_PRINCIPAL: usize = 72;
pub const SUB_POS_WITHDRAWN_AMOUNT: usize = 80;
pub const SUB_POS_WITHDRAWN: usize = 88;
pub const SUB_POS_START_SLOT: usize = 89;
pub const SUB_POS_TERMINAL_RETURNED: usize = 98;
pub const SUB_POS_TERMINAL_RETURN_SLOT: usize = 99;

fn validate_subledger_position(data: &[u8]) -> ProgramResult {
    let withdrawn = data.get(SUB_POS_WITHDRAWN).copied().unwrap_or(2);
    let terminal_returned = data.get(SUB_POS_TERMINAL_RETURNED).copied().unwrap_or(0);
    let terminal_return_slot_encoded = data
        .get(SUB_POS_TERMINAL_RETURN_SLOT..SUB_POS_TERMINAL_RETURN_SLOT + 5)
        .map(|bytes| {
            let mut encoded = [0u8; 8];
            encoded[..5].copy_from_slice(bytes);
            u64::from_le_bytes(encoded)
        })
        .unwrap_or(0);
    let principal = data
        .get(SUB_POS_PRINCIPAL..SUB_POS_PRINCIPAL + 8)
        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
        .unwrap_or(1);
    if data.get(..8) != Some(SUB_POSITION_DISC.as_slice())
        || withdrawn > 1
        || terminal_returned > 1
        || (terminal_returned == 0 && terminal_return_slot_encoded != 0)
        || (terminal_returned == 1 && (withdrawn != 1 || principal != 0))
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

/// Comparable base-unit principal from a live subledger Position. Raw shares cannot be summed across
/// pools because each pool has an independent loss/surplus-dependent share price.
pub fn read_subledger_principal(data: &[u8]) -> Result<(u128, bool), ProgramError> {
    validate_subledger_position(data)?;
    let bytes = data
        .get(SUB_POS_PRINCIPAL..SUB_POS_PRINCIPAL + 8)
        .ok_or(ProgramError::AccountDataTooSmall)?;
    let principal = u64::from_le_bytes(bytes.try_into().unwrap()) as u128;
    let withdrawn = *data
        .get(SUB_POS_WITHDRAWN)
        .ok_or(ProgramError::AccountDataTooSmall)?
        == 1;
    Ok((principal, withdrawn))
}

fn read_terminal_return_snapshot(
    data: &[u8],
) -> Result<Option<(u128, Option<u64>)>, ProgramError> {
    validate_subledger_position(data)?;
    if data.get(SUB_POS_TERMINAL_RETURNED).copied() != Some(1) {
        return Ok(None);
    }
    let principal = u64::from_le_bytes(
        data.get(SUB_POS_WITHDRAWN_AMOUNT..SUB_POS_WITHDRAWN_AMOUNT + 8)
            .ok_or(ProgramError::AccountDataTooSmall)?
            .try_into()
            .unwrap(),
    );
    if principal == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut encoded = [0u8; 8];
    encoded[..5].copy_from_slice(
        data.get(SUB_POS_TERMINAL_RETURN_SLOT..SUB_POS_TERMINAL_RETURN_SLOT + 5)
            .ok_or(ProgramError::AccountDataTooSmall)?,
    );
    let return_slot = u64::from_le_bytes(encoded).checked_sub(1);
    Ok(Some((principal as u128, return_slot)))
}

/// The subledger resets this clock whenever capital is added to the position. Reward tenure must
/// therefore start no earlier than this slot, even if the distributor stake was registered before it.
pub fn read_subledger_start_slot(data: &[u8]) -> Result<u64, ProgramError> {
    let bytes = data
        .get(SUB_POS_START_SLOT..SUB_POS_START_SLOT + 8)
        .ok_or(ProgramError::AccountDataTooSmall)?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

/// Live capital-at-risk before the separate tenure multiplier (0 if exited).
pub fn capital_points(principal: u128, withdrawn: bool) -> u128 {
    if withdrawn {
        0
    } else {
        principal
    }
}

// ===========================================================================
// percolator PortfolioAccountV16Account snapshot reads — offsets PINNED
// ===========================================================================
// Account = HEADER_LEN(16) + repr(C) PortfolioAccountV16Account { provenance_header(100), owner[32]@100,
// capital@132, pnl@148, reserved_pnl@164, residual_crystallized_loss_atoms_total@180,
// residual_spent_principal_atoms_total@196, residual_received_atoms_total@212,
// funding_long_paid_atoms_total@228, funding_long_received_atoms_total@244,
// funding_short_paid_atoms_total@260, funding_short_received_atoms_total@276, ... }.
// Absolute = 16 + within-struct. PINNED against the real struct by tests/offsets.rs
// (offset_of! + HEADER_LEN).
// Cohort 2 reads residual_received; cohort 3 reads crystallized_loss - spent.
// Cohort 4 reads funding_long_paid + funding_short_paid. We intentionally do NOT reward the funding receiving side.
// The portfolio's provenance market_group_id is the FIRST field of the struct, so it sits right after
// the percolator account header. The funding-payer cohort MUST scope to it (finding IL): funding-flow
// counters are admin-mark-manipulable on a market whose oracle the registrant controls, so a portfolio
// is only countable if it belongs to the ONE allow-listed (trusted-Pyth) genesis market the orchestrator
// bound at init (config.market_group). Without this an attacker stands up their OWN percolator market with
// an auth-mark oracle they push, self-trades to mint funding counters, and farms the COIN.
pub const OFF_PORTFOLIO_MARKET_GROUP: usize = PERC_HEADER_LEN;
pub const OFF_PORTFOLIO_OWNER: usize = PERC_HEADER_LEN + 100;
pub const OFF_PORTFOLIO_CRYSTALLIZED_LOSS: usize = PERC_HEADER_LEN + 180;
pub const OFF_PORTFOLIO_SPENT: usize = PERC_HEADER_LEN + 196;
pub const OFF_PORTFOLIO_RECEIVED: usize = PERC_HEADER_LEN + 212;
pub const OFF_PORTFOLIO_FUNDING_LONG_PAID: usize = PERC_HEADER_LEN + 228;
pub const OFF_PORTFOLIO_FUNDING_LONG_RECEIVED: usize = PERC_HEADER_LEN + 244;
pub const OFF_PORTFOLIO_FUNDING_SHORT_PAID: usize = PERC_HEADER_LEN + 260;
pub const OFF_PORTFOLIO_FUNDING_SHORT_RECEIVED: usize = PERC_HEADER_LEN + 276;

/// (residual_received, residual_crystallized_loss, residual_spent_principal) from a live Percolator
/// PortfolioAccount.
pub fn read_portfolio_residual(data: &[u8]) -> Result<(u128, u128, u128), ProgramError> {
    Ok((
        read_u128(data, OFF_PORTFOLIO_RECEIVED)?,
        read_u128(data, OFF_PORTFOLIO_CRYSTALLIZED_LOSS)?,
        read_u128(data, OFF_PORTFOLIO_SPENT)?,
    ))
}

fn residual_counter(cohort: u8, received: u128, crystallized: u128, spent: u128) -> u128 {
    if cohort == COHORT_LP {
        received
    } else {
        crystallized.saturating_sub(spent)
    }
}

/// (long_paid, long_received, short_paid, short_received) from a live percolator PortfolioAccount.
/// Reward points use only the paid counters; received counters are pinned so the side selection cannot drift.
pub fn read_portfolio_funding_flow(data: &[u8]) -> Result<(u128, u128, u128, u128), ProgramError> {
    Ok((
        read_u128(data, OFF_PORTFOLIO_FUNDING_LONG_PAID)?,
        read_u128(data, OFF_PORTFOLIO_FUNDING_LONG_RECEIVED)?,
        read_u128(data, OFF_PORTFOLIO_FUNDING_SHORT_PAID)?,
        read_u128(data, OFF_PORTFOLIO_FUNDING_SHORT_RECEIVED)?,
    ))
}

/// The portfolio cohort's funding-payer counter. Points go to funding the portfolio paid as either side.
fn funding_payer_counter(long_paid: u128, short_paid: u128) -> u128 {
    long_paid.saturating_add(short_paid)
}

#[cfg(test)]
fn validate_portfolio_identity(config: &Config, data: &[u8], owner: &Pubkey) -> ProgramResult {
    if data.len() < OFF_PORTFOLIO_OWNER + 32 {
        return Err(ProgramError::AccountDataTooSmall);
    }
    if u64::from_le_bytes(
        data.get(..8)
            .ok_or(ProgramError::AccountDataTooSmall)?
            .try_into()
            .unwrap(),
    ) != PERC_MAGIC
        || u16::from_le_bytes(
            data.get(8..10)
                .ok_or(ProgramError::AccountDataTooSmall)?
                .try_into()
                .unwrap(),
        ) != PERC_VERSION
        || data.get(10).copied() != Some(PERC_KIND_PORTFOLIO)
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if read_pubkey(data, OFF_PORTFOLIO_OWNER)? != *owner {
        return Err(ProgramError::IllegalOwner);
    }
    if !config.market_allowed(&read_pubkey(data, OFF_PORTFOLIO_MARKET_GROUP)?) {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(())
}

/// floor(log2(n)); 0 for n < 2. The residual time-weight multiplier (parity with genesis-vote's
/// floor(log2(hold_time)) and the rd's original GZ design).
fn floor_log2(n: u64) -> u128 {
    if n < 2 {
        0
    } else {
        (63 - n.leading_zeros()) as u128
    }
}

// ===========================================================================
// State
// ===========================================================================
struct Config {
    coin_mint: Pubkey,
    // VESTIGIAL post-IX_SEAL-retirement: init canonical-binds + stores these (the old seal target), but no
    // instruction reads them now — payouts are self-service from `vault`. Kept for serialized-layout stability.
    distribution_program: Pubkey,
    distribution_config: Pubkey,
    percolator_program: Pubkey,
    total_supply: u64,
    fee_support_bps: u16,
    emission_end_slot: u64,
    total_points: u128, // residual-backing cohort
    sealed: u8,
    bump: u8,
    insurance_bps: u16, // insurance cohort's share of supply (e.g. 2000 = 20%)
    insurance_total_points: u128, // insurance cohort total (capital*log-time)
    subledger_program: Pubkey, // owner of the insurance-cohort positions
    // The ONE genesis insurance pool the insurance cohort is scoped to (finding HG). An insurance
    // position from any OTHER pool of the same subledger program must not farm this genesis's COIN.
    subledger_pool: Pubkey,
    // The genesis percolator market_group the RESIDUAL cohort is scoped to (finding HI). A backing
    // ledger from any OTHER market must not farm this genesis's COIN. Pubkey::default() = unscoped.
    market_group: Pubkey,
    // SELF-SERVICE FINALIZE (replacing the cranker seal). After emission_end a permissionless
    // IX_FREEZE snapshots the cohort denominators here and stamps freeze_slot; from then on
    // register/crystallize are closed and each backer finalizes/claims their OWN deterministic share
    // (share = cohort_supply * points / frozen_*_points). freeze_slot == 0 means "not yet frozen".
    frozen_total_points: u128,
    frozen_insurance_total_points: u128,
    freeze_slot: u64,
    // The COIN vault (token account owned by this rd_config PDA) that self-service claims pay from.
    // Bound at freeze after verifying it is rd_config-owned, holds the full fixed supply, and the
    // coin_mint has no mint authority (GX/EZ) — so the supply can't be inflated under the claimers.
    // Pubkey::default() until frozen.
    vault: Pubkey,
    // Slots AFTER emission_end before the denominators lock. Legacy configs may crystallize during
    // this window. Reward epochs close crystallization at emission_end so cumulative counters cannot
    // add post-period points; their window is an operational delay before permissionless freeze.
    finalize_window: u64,
    // ---- Residual tail. `total_points`/`insurance_total_points` above are BACKING and INSURANCE;
    // these add LP/TRADER residual cohorts and the backing pool scope. trader_bps is implicit from the
    // remainder after all explicit bps, including the optional cumulative funding-payer bps appended below. ----
    backing_pool: Pubkey, // the genesis BACKING subledger pool (DOMAIN_BACKING) the backing cohort is scoped to
    backing_bps: u16,     // backing cohort supply share
    lp_bps: u16,          // LP residual cohort supply share
    lp_total_points: u128, // LP residual cohort (PortfolioAccount residual_received Δ)
    trader_total_points: u128, // trader residual cohort (crystallized_loss - spent Δ)
    frozen_lp_total_points: u128,
    frozen_trader_total_points: u128,
    // ---- market allow-list tail (finding IL+) ----
    // The funding-payer cohort counts a portfolio ONLY if its provenance market_group is allow-listed. The
    // primary entry is `market_group` above; these are 0..=MAX_EXTRA_MARKETS additional trusted markets the
    // orchestrator vetted at init. `extra_market_count` of them are live (the rest are Pubkey::default()).
    extra_market_count: u8,
    extra_markets: Vec<Pubkey>,
    // ---- Appended funding-payer tail (keeps all existing offsets stable) ----
    funding_payer_bps: u16,
    reserved_funding_payer_bps: u16,
    funding_payer_total_points: u128,
    reserved_funding_payer_total_points: u128,
    frozen_funding_payer_total_points: u128,
    reserved_frozen_funding_payer_total_points: u128,
    // Multi-epoch tail. Legacy genesis configs leave authority default and kind=0. Reward epochs are
    // canonical per (authority, coin_mint, epoch_id), bind their COIN vault at init, and may snapshot
    // either a fixed whole-mint supply or the vault balance accumulated from TWAP buybacks.
    epoch_authority: Pubkey,
    epoch_id: u64,
    emission_start_slot: u64,
    reward_supply_mode: u8,
    config_kind: u8,
    extra_insurance_pools: Vec<Pubkey>,
    extra_backing_pools: Vec<Pubkey>,
}
impl Config {
    fn deserialize(d: &[u8]) -> Result<Self, ProgramError> {
        let pre_epoch = d.len() == PRE_EPOCH_CONFIG_SIZE;
        if (!pre_epoch && d.len() < CONFIG_SIZE) || d[..8] != CONFIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let config = Config {
            coin_mint: pk(d, 8),
            distribution_program: pk(d, 40),
            distribution_config: pk(d, 72),
            percolator_program: pk(d, 104),
            total_supply: u64::from_le_bytes(d[136..144].try_into().unwrap()),
            fee_support_bps: u16::from_le_bytes(d[144..146].try_into().unwrap()),
            emission_end_slot: u64::from_le_bytes(d[146..154].try_into().unwrap()),
            total_points: u128::from_le_bytes(d[154..170].try_into().unwrap()),
            sealed: d[170],
            bump: d[171],
            insurance_bps: u16::from_le_bytes(d[172..174].try_into().unwrap()),
            insurance_total_points: u128::from_le_bytes(d[174..190].try_into().unwrap()),
            subledger_program: pk(d, 190),
            subledger_pool: pk(d, 222),
            market_group: pk(d, 254),
            frozen_total_points: u128::from_le_bytes(d[286..302].try_into().unwrap()),
            frozen_insurance_total_points: u128::from_le_bytes(d[302..318].try_into().unwrap()),
            freeze_slot: u64::from_le_bytes(d[318..326].try_into().unwrap()),
            vault: pk(d, 326),
            finalize_window: u64::from_le_bytes(d[358..366].try_into().unwrap()),
            backing_pool: pk(d, 366),
            backing_bps: u16::from_le_bytes(d[398..400].try_into().unwrap()),
            lp_bps: u16::from_le_bytes(d[400..402].try_into().unwrap()),
            lp_total_points: u128::from_le_bytes(d[402..418].try_into().unwrap()),
            trader_total_points: u128::from_le_bytes(d[418..434].try_into().unwrap()),
            frozen_lp_total_points: u128::from_le_bytes(d[434..450].try_into().unwrap()),
            frozen_trader_total_points: u128::from_le_bytes(d[450..466].try_into().unwrap()),
            extra_market_count: d[466],
            extra_markets: {
                let mut a = Vec::with_capacity(MAX_EXTRA_MARKETS);
                let mut i = 0;
                while i < MAX_EXTRA_MARKETS {
                    a.push(pk(d, 467 + i * 32));
                    i += 1;
                }
                a
            },
            funding_payer_bps: u16::from_le_bytes(
                d[CONFIG_FUNDING_TAIL_OFF..CONFIG_FUNDING_TAIL_OFF + 2]
                    .try_into()
                    .unwrap(),
            ),
            reserved_funding_payer_bps: u16::from_le_bytes(
                d[CONFIG_FUNDING_TAIL_OFF + 2..CONFIG_FUNDING_TAIL_OFF + 4]
                    .try_into()
                    .unwrap(),
            ),
            funding_payer_total_points: u128::from_le_bytes(
                d[CONFIG_FUNDING_TAIL_OFF + 4..CONFIG_FUNDING_TAIL_OFF + 20]
                    .try_into()
                    .unwrap(),
            ),
            reserved_funding_payer_total_points: u128::from_le_bytes(
                d[CONFIG_FUNDING_TAIL_OFF + 20..CONFIG_FUNDING_TAIL_OFF + 36]
                    .try_into()
                    .unwrap(),
            ),
            frozen_funding_payer_total_points: u128::from_le_bytes(
                d[CONFIG_FUNDING_TAIL_OFF + 36..CONFIG_FUNDING_TAIL_OFF + 52]
                    .try_into()
                    .unwrap(),
            ),
            reserved_frozen_funding_payer_total_points: u128::from_le_bytes(
                d[CONFIG_FUNDING_TAIL_OFF + 52..CONFIG_FUNDING_TAIL_OFF + 68]
                    .try_into()
                    .unwrap(),
            ),
            epoch_authority: if pre_epoch {
                Pubkey::default()
            } else {
                pk(d, CONFIG_EPOCH_TAIL_OFF)
            },
            epoch_id: if pre_epoch {
                0
            } else {
                u64::from_le_bytes(
                    d[CONFIG_EPOCH_TAIL_OFF + 32..CONFIG_EPOCH_TAIL_OFF + 40]
                        .try_into()
                        .unwrap(),
                )
            },
            emission_start_slot: if pre_epoch {
                0
            } else {
                u64::from_le_bytes(
                    d[CONFIG_EPOCH_TAIL_OFF + 40..CONFIG_EPOCH_TAIL_OFF + 48]
                        .try_into()
                        .unwrap(),
                )
            },
            reward_supply_mode: if pre_epoch {
                REWARD_SUPPLY_FIXED
            } else {
                d[CONFIG_EPOCH_TAIL_OFF + 48]
            },
            config_kind: if pre_epoch {
                CONFIG_KIND_LEGACY
            } else {
                d[CONFIG_EPOCH_TAIL_OFF + 49]
            },
            extra_insurance_pools: if pre_epoch {
                Vec::new()
            } else {
                let mut pools = Vec::with_capacity(MAX_EXTRA_MARKETS);
                for i in 0..MAX_EXTRA_MARKETS {
                    pools.push(pk(d, CONFIG_EXTRA_INSURANCE_POOLS_OFF + i * 32));
                }
                pools
            },
            extra_backing_pools: if pre_epoch {
                Vec::new()
            } else {
                let mut pools = Vec::with_capacity(MAX_EXTRA_MARKETS);
                for i in 0..MAX_EXTRA_MARKETS {
                    pools.push(pk(d, CONFIG_EXTRA_BACKING_POOLS_OFF + i * 32));
                }
                pools
            },
        };
        if config.config_kind > CONFIG_KIND_REWARD_EPOCH
            || config.reward_supply_mode > REWARD_SUPPLY_VAULT_BALANCE
            || config.extra_market_count as usize > MAX_EXTRA_MARKETS
            || (config.config_kind == CONFIG_KIND_LEGACY
                && config.reward_supply_mode != REWARD_SUPPLY_FIXED)
            || (config.config_kind == CONFIG_KIND_REWARD_EPOCH
                && config.epoch_authority == Pubkey::default())
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(config)
    }
    fn serialize(&self, d: &mut [u8]) {
        d[..8].copy_from_slice(&CONFIG_DISC);
        d[8..40].copy_from_slice(self.coin_mint.as_ref());
        d[40..72].copy_from_slice(self.distribution_program.as_ref());
        d[72..104].copy_from_slice(self.distribution_config.as_ref());
        d[104..136].copy_from_slice(self.percolator_program.as_ref());
        d[136..144].copy_from_slice(&self.total_supply.to_le_bytes());
        d[144..146].copy_from_slice(&self.fee_support_bps.to_le_bytes());
        d[146..154].copy_from_slice(&self.emission_end_slot.to_le_bytes());
        d[154..170].copy_from_slice(&self.total_points.to_le_bytes());
        d[170] = self.sealed;
        d[171] = self.bump;
        d[172..174].copy_from_slice(&self.insurance_bps.to_le_bytes());
        d[174..190].copy_from_slice(&self.insurance_total_points.to_le_bytes());
        d[190..222].copy_from_slice(self.subledger_program.as_ref());
        d[222..254].copy_from_slice(self.subledger_pool.as_ref());
        d[254..286].copy_from_slice(self.market_group.as_ref());
        d[286..302].copy_from_slice(&self.frozen_total_points.to_le_bytes());
        d[302..318].copy_from_slice(&self.frozen_insurance_total_points.to_le_bytes());
        d[318..326].copy_from_slice(&self.freeze_slot.to_le_bytes());
        d[326..358].copy_from_slice(self.vault.as_ref());
        d[358..366].copy_from_slice(&self.finalize_window.to_le_bytes());
        d[366..398].copy_from_slice(self.backing_pool.as_ref());
        d[398..400].copy_from_slice(&self.backing_bps.to_le_bytes());
        d[400..402].copy_from_slice(&self.lp_bps.to_le_bytes());
        d[402..418].copy_from_slice(&self.lp_total_points.to_le_bytes());
        d[418..434].copy_from_slice(&self.trader_total_points.to_le_bytes());
        d[434..450].copy_from_slice(&self.frozen_lp_total_points.to_le_bytes());
        d[450..466].copy_from_slice(&self.frozen_trader_total_points.to_le_bytes());
        d[466] = self.extra_market_count;
        for i in 0..MAX_EXTRA_MARKETS {
            let m = self.extra_markets.get(i).copied().unwrap_or_default();
            d[467 + i * 32..467 + i * 32 + 32].copy_from_slice(m.as_ref());
        }
        d[CONFIG_FUNDING_TAIL_OFF..CONFIG_FUNDING_TAIL_OFF + 2]
            .copy_from_slice(&self.funding_payer_bps.to_le_bytes());
        d[CONFIG_FUNDING_TAIL_OFF + 2..CONFIG_FUNDING_TAIL_OFF + 4]
            .copy_from_slice(&self.reserved_funding_payer_bps.to_le_bytes());
        d[CONFIG_FUNDING_TAIL_OFF + 4..CONFIG_FUNDING_TAIL_OFF + 20]
            .copy_from_slice(&self.funding_payer_total_points.to_le_bytes());
        d[CONFIG_FUNDING_TAIL_OFF + 20..CONFIG_FUNDING_TAIL_OFF + 36]
            .copy_from_slice(&self.reserved_funding_payer_total_points.to_le_bytes());
        d[CONFIG_FUNDING_TAIL_OFF + 36..CONFIG_FUNDING_TAIL_OFF + 52]
            .copy_from_slice(&self.frozen_funding_payer_total_points.to_le_bytes());
        d[CONFIG_FUNDING_TAIL_OFF + 52..CONFIG_FUNDING_TAIL_OFF + 68].copy_from_slice(
            &self
                .reserved_frozen_funding_payer_total_points
                .to_le_bytes(),
        );
        // The exact predecessor layout ends here. It is already a complete legacy
        // genesis config; continuous reward epoch metadata did not exist yet.
        if d.len() == PRE_EPOCH_CONFIG_SIZE {
            return;
        }
        d[CONFIG_EPOCH_TAIL_OFF..CONFIG_EPOCH_TAIL_OFF + 32]
            .copy_from_slice(self.epoch_authority.as_ref());
        d[CONFIG_EPOCH_TAIL_OFF + 32..CONFIG_EPOCH_TAIL_OFF + 40]
            .copy_from_slice(&self.epoch_id.to_le_bytes());
        d[CONFIG_EPOCH_TAIL_OFF + 40..CONFIG_EPOCH_TAIL_OFF + 48]
            .copy_from_slice(&self.emission_start_slot.to_le_bytes());
        d[CONFIG_EPOCH_TAIL_OFF + 48] = self.reward_supply_mode;
        d[CONFIG_EPOCH_TAIL_OFF + 49] = self.config_kind;
        for i in 0..MAX_EXTRA_MARKETS {
            let insurance_pool = self
                .extra_insurance_pools
                .get(i)
                .copied()
                .unwrap_or_default();
            d[CONFIG_EXTRA_INSURANCE_POOLS_OFF + i * 32
                ..CONFIG_EXTRA_INSURANCE_POOLS_OFF + (i + 1) * 32]
                .copy_from_slice(insurance_pool.as_ref());
            let backing_pool = self.extra_backing_pools.get(i).copied().unwrap_or_default();
            d[CONFIG_EXTRA_BACKING_POOLS_OFF + i * 32
                ..CONFIG_EXTRA_BACKING_POOLS_OFF + (i + 1) * 32]
                .copy_from_slice(backing_pool.as_ref());
        }
    }
    /// Is `m` an allow-listed (orchestrator-vetted trusted-Pyth) market for the funding-payer cohort? The
    /// primary `market_group` plus the first `extra_market_count` extras. Default is never allowed.
    fn market_allowed(&self, m: &Pubkey) -> bool {
        if *m == Pubkey::default() {
            return false;
        }
        if *m == self.market_group {
            return true;
        }
        let count = core::cmp::min(self.extra_market_count as usize, self.extra_markets.len());
        self.extra_markets[..count].contains(m)
    }
    fn pool_allowed(&self, cohort: u8, pool: &Pubkey) -> bool {
        if *pool == Pubkey::default() {
            return false;
        }
        let (primary, extras) = match cohort {
            COHORT_INSURANCE => (self.subledger_pool, &self.extra_insurance_pools),
            COHORT_BACKING => (self.backing_pool, &self.extra_backing_pools),
            _ => return false,
        };
        if *pool == primary {
            return true;
        }
        let count = core::cmp::min(self.extra_market_count as usize, extras.len());
        extras[..count].contains(pool)
    }
    /// Trader bps = the remainder after all explicit cohort bps.
    fn trader_bps(&self) -> u16 {
        (BPS_DENOMINATOR as u16)
            .saturating_sub(self.insurance_bps)
            .saturating_sub(self.backing_bps)
            .saturating_sub(self.lp_bps)
            .saturating_sub(self.funding_payer_bps)
    }
    /// COIN supply allocated to a cohort.
    fn cohort_supply(&self, cohort: u8) -> u64 {
        let bps = match cohort {
            COHORT_INSURANCE => self.insurance_bps,
            COHORT_BACKING => self.backing_bps,
            COHORT_LP => self.lp_bps,
            COHORT_TRADER => self.trader_bps(),
            COHORT_FUNDING_PAYER => self.funding_payer_bps,
            _ => 0,
        } as u128;
        ((self.total_supply as u128) * bps / BPS_DENOMINATOR as u128) as u64
    }
    /// Live running point total for a cohort (mutated in register/crystallize).
    fn cohort_points_mut(&mut self, cohort: u8) -> &mut u128 {
        match cohort {
            COHORT_INSURANCE => &mut self.insurance_total_points,
            COHORT_BACKING => &mut self.total_points,
            COHORT_LP => &mut self.lp_total_points,
            COHORT_TRADER => &mut self.trader_total_points,
            COHORT_FUNDING_PAYER => &mut self.funding_payer_total_points,
            _ => &mut self.reserved_funding_payer_total_points,
        }
    }
    /// Frozen denominator for a cohort (snapshotted at freeze; used by claim).
    fn frozen_cohort_points(&self, cohort: u8) -> u128 {
        match cohort {
            COHORT_INSURANCE => self.frozen_insurance_total_points,
            COHORT_BACKING => self.frozen_total_points,
            COHORT_LP => self.frozen_lp_total_points,
            COHORT_TRADER => self.frozen_trader_total_points,
            COHORT_FUNDING_PAYER => self.frozen_funding_payer_total_points,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy)]
struct PortfolioArchive {
    percolator_program: Pubkey,
    market: Pubkey,
    owner: Pubkey,
    portfolio: Pubkey,
    residual_crystallized_loss: u128,
    residual_spent_principal: u128,
    residual_received: u128,
    funding_long_paid: u128,
    funding_long_received: u128,
    funding_short_paid: u128,
    funding_short_received: u128,
    generation: u64,
    archived_slot: u64,
}

impl PortfolioArchive {
    fn empty(
        percolator_program: Pubkey,
        market: Pubkey,
        owner: Pubkey,
        portfolio: Pubkey,
    ) -> Self {
        Self {
            percolator_program,
            market,
            owner,
            portfolio,
            residual_crystallized_loss: 0,
            residual_spent_principal: 0,
            residual_received: 0,
            funding_long_paid: 0,
            funding_long_received: 0,
            funding_short_paid: 0,
            funding_short_received: 0,
            generation: 0,
            archived_slot: 0,
        }
    }

    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() != PORTFOLIO_ARCHIVE_SIZE || data[..8] != PORTFOLIO_ARCHIVE_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            percolator_program: pk(data, 8),
            market: pk(data, 40),
            owner: pk(data, 72),
            portfolio: pk(data, 104),
            residual_crystallized_loss: u128::from_le_bytes(
                data[136..152].try_into().unwrap(),
            ),
            residual_spent_principal: u128::from_le_bytes(
                data[152..168].try_into().unwrap(),
            ),
            residual_received: u128::from_le_bytes(data[168..184].try_into().unwrap()),
            funding_long_paid: u128::from_le_bytes(data[184..200].try_into().unwrap()),
            funding_long_received: u128::from_le_bytes(data[200..216].try_into().unwrap()),
            funding_short_paid: u128::from_le_bytes(data[216..232].try_into().unwrap()),
            funding_short_received: u128::from_le_bytes(data[232..248].try_into().unwrap()),
            generation: u64::from_le_bytes(data[248..256].try_into().unwrap()),
            archived_slot: u64::from_le_bytes(data[256..264].try_into().unwrap()),
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&PORTFOLIO_ARCHIVE_DISC);
        data[8..40].copy_from_slice(self.percolator_program.as_ref());
        data[40..72].copy_from_slice(self.market.as_ref());
        data[72..104].copy_from_slice(self.owner.as_ref());
        data[104..136].copy_from_slice(self.portfolio.as_ref());
        data[136..152].copy_from_slice(&self.residual_crystallized_loss.to_le_bytes());
        data[152..168].copy_from_slice(&self.residual_spent_principal.to_le_bytes());
        data[168..184].copy_from_slice(&self.residual_received.to_le_bytes());
        data[184..200].copy_from_slice(&self.funding_long_paid.to_le_bytes());
        data[200..216].copy_from_slice(&self.funding_long_received.to_le_bytes());
        data[216..232].copy_from_slice(&self.funding_short_paid.to_le_bytes());
        data[232..248].copy_from_slice(&self.funding_short_received.to_le_bytes());
        data[248..256].copy_from_slice(&self.generation.to_le_bytes());
        data[256..264].copy_from_slice(&self.archived_slot.to_le_bytes());
    }

    fn validate_identity(
        &self,
        percolator_program: &Pubkey,
        market: &Pubkey,
        owner: &Pubkey,
        portfolio: &Pubkey,
    ) -> ProgramResult {
        if self.percolator_program != *percolator_program
            || self.market != *market
            || self.owner != *owner
            || self.portfolio != *portfolio
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    fn add_live(
        &mut self,
        live: &percolator_accounting::PortfolioRewardSnapshot,
        archived_slot: u64,
    ) -> ProgramResult {
        self.validate_identity(
            &self.percolator_program,
            &Pubkey::new_from_array(live.market_group),
            &Pubkey::new_from_array(live.owner),
            &Pubkey::new_from_array(live.portfolio),
        )?;
        macro_rules! add_counter {
            ($field:ident) => {
                self.$field = self
                    .$field
                    .checked_add(live.$field)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
            };
        }
        add_counter!(residual_crystallized_loss);
        add_counter!(residual_spent_principal);
        add_counter!(residual_received);
        add_counter!(funding_long_paid);
        add_counter!(funding_long_received);
        add_counter!(funding_short_paid);
        add_counter!(funding_short_received);
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        self.archived_slot = archived_slot;
        Ok(())
    }

    fn totals_with_live(
        &self,
        live: Option<&percolator_accounting::PortfolioRewardSnapshot>,
    ) -> Result<percolator_accounting::PortfolioRewardSnapshot, ProgramError> {
        let mut totals = percolator_accounting::PortfolioRewardSnapshot {
            market_group: self.market.to_bytes(),
            portfolio: self.portfolio.to_bytes(),
            owner: self.owner.to_bytes(),
            residual_crystallized_loss: self.residual_crystallized_loss,
            residual_spent_principal: self.residual_spent_principal,
            residual_received: self.residual_received,
            funding_long_paid: self.funding_long_paid,
            funding_long_received: self.funding_long_received,
            funding_short_paid: self.funding_short_paid,
            funding_short_received: self.funding_short_received,
        };
        if let Some(live) = live {
            self.validate_identity(
                &self.percolator_program,
                &Pubkey::new_from_array(live.market_group),
                &Pubkey::new_from_array(live.owner),
                &Pubkey::new_from_array(live.portfolio),
            )?;
            macro_rules! add_counter {
                ($field:ident) => {
                    totals.$field = totals
                        .$field
                        .checked_add(live.$field)
                        .ok_or(ProgramError::ArithmeticOverflow)?;
                };
            }
            add_counter!(residual_crystallized_loss);
            add_counter!(residual_spent_principal);
            add_counter!(residual_received);
            add_counter!(funding_long_paid);
            add_counter!(funding_long_received);
            add_counter!(funding_short_paid);
            add_counter!(funding_short_received);
        }
        Ok(totals)
    }
}

struct Stake {
    config: Pubkey,
    owner: Pubkey,
    backing_ledger: Pubkey,
    recipient: Pubkey,
    residual_snap: u128,
    // LOAD-BEARING live-cap snapshot. For capital cohorts this is crystallized live principal. For residual
    // cohorts it is the realized `net_delta`; claim scales the payout down if the live value fell.
    // Repurposed from the superseded fee-cap design. MUST be preserved across crystallize/freeze.
    earnings_snap: u128,
    start_slot: u64,
    points: u128,
    bump: u8,
    // `backing_ledger` is the linked subledger position for insurance/backing, or the linked Percolator
    // portfolio for LP/trader/funding-payer cohorts.
    cohort: u8,
    // Capital cohorts store their crystallization slot here so claim can reject tenure restored by a later
    // top-up. Trader residual stores the spent-counter snapshot. Funding-payer holds 0.
    eligible_accum: u128,
    // Self-service claim: set true when this stake's COIN share has been paid, so it can't be
    // double-claimed.
    claimed: bool,
}
impl Stake {
    fn deserialize(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() < STAKE_SIZE || d[..8] != STAKE_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Stake {
            config: pk(d, 8),
            owner: pk(d, 40),
            backing_ledger: pk(d, 72),
            recipient: pk(d, 104),
            residual_snap: u128::from_le_bytes(d[136..152].try_into().unwrap()),
            earnings_snap: u128::from_le_bytes(d[152..168].try_into().unwrap()),
            start_slot: u64::from_le_bytes(d[168..176].try_into().unwrap()),
            points: u128::from_le_bytes(d[176..192].try_into().unwrap()),
            bump: d[192],
            cohort: d[193],
            eligible_accum: u128::from_le_bytes(d[194..210].try_into().unwrap()),
            claimed: d[210] != 0,
        })
    }
    fn serialize(&self, d: &mut [u8]) {
        d[..8].copy_from_slice(&STAKE_DISC);
        d[8..40].copy_from_slice(self.config.as_ref());
        d[40..72].copy_from_slice(self.owner.as_ref());
        d[72..104].copy_from_slice(self.backing_ledger.as_ref());
        d[104..136].copy_from_slice(self.recipient.as_ref());
        d[136..152].copy_from_slice(&self.residual_snap.to_le_bytes());
        d[152..168].copy_from_slice(&self.earnings_snap.to_le_bytes());
        d[168..176].copy_from_slice(&self.start_slot.to_le_bytes());
        d[176..192].copy_from_slice(&self.points.to_le_bytes());
        d[192] = self.bump;
        d[193] = self.cohort;
        d[194..210].copy_from_slice(&self.eligible_accum.to_le_bytes());
        d[210] = self.claimed as u8;
    }
}

fn pk(d: &[u8], off: usize) -> Pubkey {
    Pubkey::new_from_array(d[off..off + 32].try_into().unwrap())
}

pub fn portfolio_archive_address(
    percolator_program: &Pubkey,
    market: &Pubkey,
    owner: &Pubkey,
    portfolio: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            PORTFOLIO_ARCHIVE_SEED,
            percolator_program.as_ref(),
            market.as_ref(),
            owner.as_ref(),
            portfolio.as_ref(),
        ],
        &id(),
    )
}

fn read_live_portfolio_snapshot(
    config: &Config,
    portfolio_key: &Pubkey,
    owner: &Pubkey,
    data: &[u8],
) -> Result<percolator_accounting::PortfolioRewardSnapshot, ProgramError> {
    let snapshot = percolator_accounting::read_portfolio_reward_snapshot(
        data,
        &portfolio_key.to_bytes(),
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;
    if snapshot.owner != owner.to_bytes()
        || !config.market_allowed(&Pubkey::new_from_array(snapshot.market_group))
    {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(snapshot)
}

fn archive_for_identity(
    program_id: &Pubkey,
    archive_account: &AccountInfo,
    percolator_program: &Pubkey,
    market: &Pubkey,
    owner: &Pubkey,
    portfolio: &Pubkey,
) -> Result<(PortfolioArchive, u8, bool), ProgramError> {
    let (expected, bump) = portfolio_archive_address(percolator_program, market, owner, portfolio);
    if *archive_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if archive_account.data_len() == 0 {
        if *archive_account.owner != solana_program::system_program::ID {
            return Err(ProgramError::IllegalOwner);
        }
        return Ok((
            PortfolioArchive::empty(*percolator_program, *market, *owner, *portfolio),
            bump,
            false,
        ));
    }
    if archive_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let archive = PortfolioArchive::deserialize(&archive_account.try_borrow_data()?)?;
    archive.validate_identity(percolator_program, market, owner, portfolio)?;
    Ok((archive, bump, true))
}

fn archive_key_matches_allowed_market(
    config: &Config,
    archive_key: &Pubkey,
    owner: &Pubkey,
    portfolio: &Pubkey,
) -> bool {
    let matches_market = |market: &Pubkey| {
        *market != Pubkey::default()
            && portfolio_archive_address(
                &config.percolator_program,
                market,
                owner,
                portfolio,
            )
            .0
                == *archive_key
    };
    if matches_market(&config.market_group) {
        return true;
    }
    let count = core::cmp::min(config.extra_market_count as usize, config.extra_markets.len());
    config.extra_markets[..count].iter().any(matches_market)
}

fn portfolio_totals(
    program_id: &Pubkey,
    config: &Config,
    owner: &Pubkey,
    portfolio: &AccountInfo,
    archive_account: &AccountInfo,
) -> Result<percolator_accounting::PortfolioRewardSnapshot, ProgramError> {
    if portfolio.data_len() != 0 {
        if *portfolio.owner != config.percolator_program {
            return Err(ProgramError::IllegalOwner);
        }
        let live = read_live_portfolio_snapshot(
            config,
            portfolio.key,
            owner,
            &portfolio.try_borrow_data()?,
        )?;
        let market = Pubkey::new_from_array(live.market_group);
        let (archive, _, _) = archive_for_identity(
            program_id,
            archive_account,
            &config.percolator_program,
            &market,
            owner,
            portfolio.key,
        )?;
        return archive.totals_with_live(Some(&live));
    }

    if archive_account.owner != program_id || archive_account.data_len() == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let archive = PortfolioArchive::deserialize(&archive_account.try_borrow_data()?)?;
    if archive.percolator_program != config.percolator_program
        || archive.owner != *owner
        || archive.portfolio != *portfolio.key
        || !config.market_allowed(&archive.market)
    {
        return Err(ProgramError::IllegalOwner);
    }
    let (expected, _) = portfolio_archive_address(
        &archive.percolator_program,
        &archive.market,
        &archive.owner,
        &archive.portfolio,
    );
    if *archive_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    archive.totals_with_live(None)
}

fn config_seeds<'a>(coin_mint: &'a Pubkey) -> [&'a [u8]; 2] {
    [b"rd_config", coin_mint.as_ref()]
}

fn stake_family(cohort: u8) -> Result<u8, ProgramError> {
    match cohort {
        COHORT_INSURANCE | COHORT_BACKING | COHORT_LP | COHORT_FUNDING_PAYER => Ok(cohort),
        // LP and trader are alternative views of the same residual-flow economics,
        // so they deliberately collide for one linked portfolio.
        COHORT_TRADER => Ok(COHORT_LP),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn stake_seeds<'a>(
    config: &'a Pubkey,
    owner: &'a Pubkey,
    linked: &'a Pubkey,
    family: &'a [u8; 1],
) -> [&'a [u8]; 5] {
    [
        b"rd_stake",
        config.as_ref(),
        owner.as_ref(),
        linked.as_ref(),
        family,
    ]
}

// ===========================================================================
#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (tag, rest) = data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        IX_INIT => init(program_id, accounts, rest),
        IX_REGISTER_START => register_start(program_id, accounts, rest),
        IX_CRYSTALLIZE => crystallize(program_id, accounts),
        IX_FREEZE => freeze(program_id, accounts),
        IX_CLAIM => claim(program_id, accounts),
        IX_INIT_REWARD_EPOCH => init_reward_epoch(program_id, accounts, rest),
        IX_ARCHIVE_PORTFOLIO => archive_portfolio(program_id, accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn create_pda<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system: &AccountInfo<'a>,
    program_id: &Pubkey,
    seeds: &[&[u8]],
    size: usize,
) -> ProgramResult {
    // Robust create (parity with distribution/gv): a front-runner can transfer lamports to the canonical PDA
    // before init/register; a naive create_account then fails on the funded account, permanently bricking the
    // rd_config (the whole residual distribution) or denying a victim their stake. So adopt a prefunded PDA:
    // top up to rent if short, then allocate + assign — never create_account on a possibly-funded account.
    let rent = Rent::get()?.minimum_balance(size);
    let current = target.lamports();
    if current < rent {
        invoke(
            &system_instruction::transfer(payer.key, target.key, rent - current),
            &[payer.clone(), target.clone(), system.clone()],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(target.key, size as u64),
        &[target.clone(), system.clone()],
        &[seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(target.key, program_id),
        &[target.clone(), system.clone()],
        &[seeds],
    )
}

// archive_portfolio accounts:
// [payer(s,w), controller_pda(s), governance, archive_pda(w), market,
//  portfolio, percolator_program, system]
//
// The fixed market controller invokes this immediately before Percolator's
// terminal ClosePortfolio CPI in the same transaction. If that close rejects,
// the archive update rolls back. No token account is accepted and this program
// cannot move collateral; it only checked-adds authenticated monotonic telemetry.
fn archive_portfolio(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let governance = next_account_info(iter)?;
    let archive_account = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let portfolio = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system = next_account_info(iter)?;
    if iter.next().is_some()
        || !payer.is_signer
        || !payer.is_writable
        || !controller.is_signer
        || !archive_account.is_writable
        || *system.key != solana_program::system_program::ID
        || !percolator_program.executable
        || market.owner != percolator_program.key
        || portfolio.owner != percolator_program.key
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let expected_controller = Pubkey::find_program_address(
        &[
            MARKET_CONTROLLER_SEED,
            governance.key.as_ref(),
            market.key.as_ref(),
            percolator_program.key.as_ref(),
        ],
        &MARKET_CONTROLLER_PROGRAM_ID,
    )
    .0;
    if *controller.key != expected_controller {
        return Err(ProgramError::InvalidSeeds);
    }

    let live = percolator_accounting::read_portfolio_reward_snapshot(
        &portfolio.try_borrow_data()?,
        &portfolio.key.to_bytes(),
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;
    if live.market_group != market.key.to_bytes() {
        return Err(ProgramError::InvalidAccountData);
    }
    let owner = Pubkey::new_from_array(live.owner);
    let (mut archive, bump, exists) = archive_for_identity(
        program_id,
        archive_account,
        percolator_program.key,
        market.key,
        &owner,
        portfolio.key,
    )?;
    if !exists {
        let bump_seed = [bump];
        let seeds: [&[u8]; 6] = [
            PORTFOLIO_ARCHIVE_SEED,
            percolator_program.key.as_ref(),
            market.key.as_ref(),
            owner.as_ref(),
            portfolio.key.as_ref(),
            &bump_seed,
        ];
        create_pda(
            payer,
            archive_account,
            system,
            program_id,
            &seeds,
            PORTFOLIO_ARCHIVE_SIZE,
        )?;
    }
    archive.add_live(&live, Clock::get()?.slot)?;
    archive.serialize(&mut archive_account.try_borrow_mut_data()?);
    Ok(())
}

// init accounts: [payer(s,w), coin_mint, distribution_program, distribution_config,
//   percolator_program, subledger_program, config(pda,w), system, coin_mint_authority(s)]
// data: total_supply(u64), fee_support_bps(u16), emission_end_slot(u64), insurance_bps(u16)
fn init(program_id: &Pubkey, accounts: &[AccountInfo], mut data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let distribution_program = next_account_info(iter)?;
    let distribution_config = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let subledger_program = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let system = next_account_info(iter)?;
    let coin_mint_authority = next_account_info(iter)?;

    // Base wire: total_supply, emission_end, insurance_bps, backing_bps, lp_bps, finalize_window,
    // subledger_pool (insurance), backing_pool, market_group.
    // Optional trailing bytes:
    //   none: default anti-wash fee, no funding-payer allocation
    //   u16: anti-wash fee, no funding-payer allocation
    //   u16,u16: anti-wash fee, cumulative funding_payer_bps
    //   u16,u16,u16: legacy anti-wash fee, old long_bps, old short_bps; old bps are summed
    let total_supply = take_u64(&mut data)?;
    let emission_end_slot = take_u64(&mut data)?;
    let insurance_bps = take_u16(&mut data)?;
    let backing_bps = take_u16(&mut data)?;
    let lp_bps = take_u16(&mut data)?;
    let finalize_window = take_u64(&mut data)?;
    let subledger_pool = take_pubkey(&mut data)?; // insurance pool (DOMAIN_INSURANCE), finding HG scope
    let backing_pool = take_pubkey(&mut data)?; // backing pool (DOMAIN_BACKING) scope
    let market_group = take_pubkey(&mut data)?; // primary allow-listed market (funding-payer scope, finding IL)
                                                // Market allow-list tail (finding IL+): a u8 count followed by that many ADDITIONAL trusted-Pyth market
                                                // pubkeys the orchestrator vetted. Bounded by MAX_EXTRA_MARKETS; each must be a real, distinct key.
    let extra_market_count = *data.first().ok_or(ProgramError::InvalidInstructionData)?;
    data = &data[1..];
    if extra_market_count as usize > MAX_EXTRA_MARKETS {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut extra_markets = Vec::with_capacity(MAX_EXTRA_MARKETS);
    for _ in 0..extra_market_count as usize {
        let m = take_pubkey(&mut data)?;
        if m == Pubkey::default() || m == market_group || extra_markets.contains(&m) {
            return Err(ProgramError::InvalidInstructionData); // no default / duplicate allow-list entries
        }
        extra_markets.push(m);
    }
    // OPTIONAL trailing residual_fee_bps (u16, finding NZ): the anti-wash fee skimmed from portfolio-flow
    // claims. Absent (no trailing bytes) = DEFAULT_FEE_SUPPORT_BPS; explicit 0 remains allowed. A four-byte
    // tail appends the cumulative funding-payer cohort bps. The old six-byte long/short-payer tail remains
    // accepted, but those two bps are summed into ONE cumulative funding-payer allocation.
    let (residual_fee_bps, funding_payer_bps) = match data.len() {
        0 => (DEFAULT_FEE_SUPPORT_BPS, 0),
        2 => (take_u16(&mut data)?, 0),
        4 => {
            let fee = take_u16(&mut data)?;
            let funding = take_u16(&mut data)?;
            (fee, funding)
        }
        6 => {
            let fee = take_u16(&mut data)?;
            let long = take_u16(&mut data)?;
            let short = take_u16(&mut data)?;
            let funding = (long as u32)
                .checked_add(short as u32)
                .ok_or(ProgramError::InvalidInstructionData)?;
            if funding > BPS_DENOMINATOR as u32 {
                return Err(ProgramError::InvalidInstructionData);
            }
            (fee, funding as u16)
        }
        _ => return Err(ProgramError::InvalidInstructionData),
    };
    if residual_fee_bps > BPS_DENOMINATOR as u16 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !data.is_empty() || !payer.is_signer || total_supply == 0 || finalize_window == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if emission_end_slot.checked_add(finalize_window).is_none() {
        return Err(ProgramError::InvalidInstructionData);
    }
    // Explicit cohort shares must not exceed 100%; trader takes the remainder.
    let explicit_bps_sum = (insurance_bps as u32)
        + (backing_bps as u32)
        + (lp_bps as u32)
        + (funding_payer_bps as u32);
    if explicit_bps_sum > BPS_DENOMINATOR as u32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    // A cohort with a share MUST be scoped to its concrete pool, else a position from any other pool of
    // the same subledger program could farm this genesis's COIN.
    if insurance_bps > 0 && subledger_pool == Pubkey::default() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if backing_bps > 0 && backing_pool == Pubkey::default() {
        return Err(ProgramError::InvalidInstructionData);
    }
    // Capital reward scopes are segregated even though both witnesses are Subledger positions.
    // Reusing one non-default pool would let the same position derive distinct insurance and
    // backing stake families and consume both cohort allocations for one principal balance.
    if subledger_pool != Pubkey::default() && subledger_pool == backing_pool {
        return Err(ProgramError::InvalidInstructionData);
    }
    // Portfolio-flow cohorts read Percolator counters that are admin-mark-manipulable on a market whose oracle
    // the registrant controls, so any nonzero portfolio-flow allocation MUST be scoped to an allow-listed market.
    let trader_bps = (BPS_DENOMINATOR as u32).saturating_sub(explicit_bps_sum);
    if (lp_bps > 0 || trader_bps > 0 || funding_payer_bps > 0) && market_group == Pubkey::default()
    {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (expected, bump) = Pubkey::find_program_address(&config_seeds(coin_mint.key), program_id);
    if *config_account.key != expected || config_account.data_len() != 0 {
        return Err(ProgramError::InvalidSeeds);
    }
    // Pin the distribution program (finding HK): a fake program would let a front-runner squat with a
    // canonical-looking-but-foreign distribution_config and brick the real COIN distribution at seal.
    if *distribution_program.key != DISTRIBUTION_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    // Bind distribution_config to the canonical PDA(["dist_config", coin_mint, rd_config,
    // claim_window]) under the distribution program (finding HC; parity with genesis-vote finding R).
    // rd_config (= `expected`) is the distribution authority, so the ONLY config rd can ever seal is
    // the one at this PDA. The seal path is retired, so keep this vestigial dependency pinned to the
    // historical default claim window instead of widening residual init policy.
    // Without this, a front-runner could squat this canonical (per-coin_mint) rd_config with a foreign
    // distribution_config; since rd_config can't be re-initialized, seal would forever target the
    // foreign config and the real COIN-holding distribution could never be sealed -> DOS.
    let distribution_claim_window = DISTRIBUTION_CLAIM_WINDOW_SLOTS.to_le_bytes();
    let (expected_dist, _) = Pubkey::find_program_address(
        &[
            b"dist_config",
            coin_mint.key.as_ref(),
            expected.as_ref(),
            &distribution_claim_window,
        ],
        distribution_program.key,
    );
    if *distribution_config.key != expected_dist {
        return Err(ProgramError::InvalidSeeds);
    }
    // The rd_config PDA is canonical per coin mint, so init must be authorized by the current SPL mint authority.
    // This prevents a first-mover from squatting PDA(["rd_config", coin_mint]) with attacker-chosen parameters.
    if coin_mint.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let mint = spl_token::state::Mint::unpack(&coin_mint.try_borrow_data()?)?;
    if mint.mint_authority != COption::Some(*coin_mint_authority.key)
        || !coin_mint_authority.is_signer
    {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let bump_arr = [bump];
    let seeds: [&[u8]; 3] = [b"rd_config", coin_mint.key.as_ref(), &bump_arr];
    create_pda(
        payer,
        config_account,
        system,
        program_id,
        &seeds,
        CONFIG_SIZE,
    )?;
    Config {
        coin_mint: *coin_mint.key,
        distribution_program: *distribution_program.key,
        distribution_config: *distribution_config.key,
        percolator_program: *percolator_program.key,
        total_supply,
        fee_support_bps: residual_fee_bps,
        emission_end_slot,
        total_points: 0, // BACKING cohort
        sealed: 0,
        bump,
        insurance_bps,
        insurance_total_points: 0,
        subledger_program: *subledger_program.key,
        subledger_pool,
        market_group,
        frozen_total_points: 0,
        frozen_insurance_total_points: 0,
        freeze_slot: 0,
        vault: Pubkey::default(),
        finalize_window,
        backing_pool,
        backing_bps,
        lp_bps,
        lp_total_points: 0,
        trader_total_points: 0,
        frozen_lp_total_points: 0,
        frozen_trader_total_points: 0,
        extra_market_count,
        extra_markets,
        funding_payer_bps,
        reserved_funding_payer_bps: 0,
        funding_payer_total_points: 0,
        reserved_funding_payer_total_points: 0,
        frozen_funding_payer_total_points: 0,
        reserved_frozen_funding_payer_total_points: 0,
        epoch_authority: Pubkey::default(),
        epoch_id: 0,
        emission_start_slot: 0,
        reward_supply_mode: REWARD_SUPPLY_FIXED,
        config_kind: CONFIG_KIND_LEGACY,
        extra_insurance_pools: Vec::new(),
        extra_backing_pools: Vec::new(),
    }
    .serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// init_reward_epoch accounts:
// [payer(s,w), authority(s), coin_mint, percolator_program, subledger_program,
//  config(pda,w), vault, system]
//
// data: epoch_id(u64), emission_start(u64), emission_end(u64), expected_reward_supply(u64),
// insurance_bps(u16), backing_bps(u16), lp_bps(u16), funding_payer_bps(u16),
// finalize_window(u64), fee_bps(u16), market_count(u8),
// market_count * [market, insurance_pool, backing_pool].
//
// `expected_reward_supply == 0` selects a TWAP-funded epoch: freeze snapshots the canonical
// vault's actual COIN balance. A nonzero value selects the genesis/full-mint invariant. Both modes
// share every subsequent instruction and can never move Percolator collateral or subledger assets.
fn init_reward_epoch(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    mut data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let subledger_program = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let system = next_account_info(iter)?;

    let epoch_id = take_u64(&mut data)?;
    let emission_start_slot = take_u64(&mut data)?;
    let emission_end_slot = take_u64(&mut data)?;
    let expected_reward_supply = take_u64(&mut data)?;
    let insurance_bps = take_u16(&mut data)?;
    let backing_bps = take_u16(&mut data)?;
    let lp_bps = take_u16(&mut data)?;
    let funding_payer_bps = take_u16(&mut data)?;
    let finalize_window = take_u64(&mut data)?;
    let fee_support_bps = take_u16(&mut data)?;
    let market_count = *data.first().ok_or(ProgramError::InvalidInstructionData)? as usize;
    data = &data[1..];

    if !payer.is_signer
        || !authority.is_signer
        || *authority.key == Pubkey::default()
        || *system.key != solana_program::system_program::ID
        || market_count == 0
        || market_count > MAX_REWARD_EPOCH_MARKETS
        || finalize_window == 0
        || fee_support_bps > BPS_DENOMINATOR as u16
        || emission_end_slot <= emission_start_slot
        || emission_end_slot.checked_add(finalize_window).is_none()
        || Clock::get()?.slot > emission_start_slot
    {
        return Err(ProgramError::InvalidInstructionData);
    }
    let explicit_bps_sum = (insurance_bps as u32)
        .checked_add(backing_bps as u32)
        .and_then(|v| v.checked_add(lp_bps as u32))
        .and_then(|v| v.checked_add(funding_payer_bps as u32))
        .ok_or(ProgramError::InvalidInstructionData)?;
    if explicit_bps_sum > BPS_DENOMINATOR as u32 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let mut markets = Vec::with_capacity(market_count);
    let mut insurance_pools = Vec::with_capacity(market_count);
    let mut backing_pools = Vec::with_capacity(market_count);
    for _ in 0..market_count {
        let market = take_pubkey(&mut data)?;
        let insurance_pool = take_pubkey(&mut data)?;
        let backing_pool = take_pubkey(&mut data)?;
        // Pool domains are globally disjoint across the epoch, not just within one market tuple.
        // Otherwise one position can derive separate insurance/backing stakes and claim both slices.
        if market == Pubkey::default()
            || markets.contains(&market)
            || (insurance_pool != Pubkey::default()
                && (insurance_pools.contains(&insurance_pool)
                    || backing_pools.contains(&insurance_pool)))
            || (backing_pool != Pubkey::default()
                && (backing_pools.contains(&backing_pool)
                    || insurance_pools.contains(&backing_pool)))
            || (insurance_pool != Pubkey::default() && insurance_pool == backing_pool)
        {
            return Err(ProgramError::InvalidInstructionData);
        }
        markets.push(market);
        insurance_pools.push(insurance_pool);
        backing_pools.push(backing_pool);
    }
    if !data.is_empty()
        || (insurance_bps > 0
            && !insurance_pools
                .iter()
                .any(|pool| *pool != Pubkey::default()))
        || (backing_bps > 0 && !backing_pools.iter().any(|pool| *pool != Pubkey::default()))
    {
        return Err(ProgramError::InvalidInstructionData);
    }

    let epoch_bytes = epoch_id.to_le_bytes();
    let (expected_config, bump) = Pubkey::find_program_address(
        &[
            b"rd_epoch",
            authority.key.as_ref(),
            coin_mint.key.as_ref(),
            &epoch_bytes,
        ],
        program_id,
    );
    if *config_account.key != expected_config || config_account.data_len() != 0 {
        return Err(ProgramError::InvalidSeeds);
    }
    if coin_mint.owner != &spl_token::ID || vault.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let _mint = spl_token::state::Mint::unpack(&coin_mint.try_borrow_data()?)?;
    let vault_state = spl_token::state::Account::unpack(&vault.try_borrow_data()?)?;
    if vault_state.state != spl_token::state::AccountState::Initialized
        || vault_state.owner != expected_config
        || vault_state.mint != *coin_mint.key
        || vault_state.delegate.is_some()
        || vault_state.delegated_amount != 0
        || vault_state.close_authority.is_some()
        || vault_state.is_native.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let bump_arr = [bump];
    let seeds: [&[u8]; 5] = [
        b"rd_epoch",
        authority.key.as_ref(),
        coin_mint.key.as_ref(),
        &epoch_bytes,
        &bump_arr,
    ];
    create_pda(
        payer,
        config_account,
        system,
        program_id,
        &seeds,
        CONFIG_SIZE,
    )?;

    let primary_market = markets[0];
    let primary_insurance_pool = insurance_pools[0];
    let primary_backing_pool = backing_pools[0];
    let extra_market_count = (market_count - 1) as u8;
    Config {
        coin_mint: *coin_mint.key,
        distribution_program: DISTRIBUTION_PROGRAM_ID,
        distribution_config: Pubkey::default(),
        percolator_program: *percolator_program.key,
        total_supply: expected_reward_supply,
        fee_support_bps,
        emission_end_slot,
        total_points: 0,
        sealed: 0,
        bump,
        insurance_bps,
        insurance_total_points: 0,
        subledger_program: *subledger_program.key,
        subledger_pool: primary_insurance_pool,
        market_group: primary_market,
        frozen_total_points: 0,
        frozen_insurance_total_points: 0,
        freeze_slot: 0,
        vault: *vault.key,
        finalize_window,
        backing_pool: primary_backing_pool,
        backing_bps,
        lp_bps,
        lp_total_points: 0,
        trader_total_points: 0,
        frozen_lp_total_points: 0,
        frozen_trader_total_points: 0,
        extra_market_count,
        extra_markets: markets.into_iter().skip(1).collect(),
        funding_payer_bps,
        reserved_funding_payer_bps: 0,
        funding_payer_total_points: 0,
        reserved_funding_payer_total_points: 0,
        frozen_funding_payer_total_points: 0,
        reserved_frozen_funding_payer_total_points: 0,
        epoch_authority: *authority.key,
        epoch_id,
        emission_start_slot,
        reward_supply_mode: if expected_reward_supply == 0 {
            REWARD_SUPPLY_VAULT_BALANCE
        } else {
            REWARD_SUPPLY_FIXED
        },
        config_kind: CONFIG_KIND_REWARD_EPOCH,
        extra_insurance_pools: insurance_pools.into_iter().skip(1).collect(),
        extra_backing_pools: backing_pools.into_iter().skip(1).collect(),
    }
    .serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// register_start accounts: [payer(s,w), config, owner, recipient, linked, stake(pda,w), system,
//   portfolio_archive (portfolio cohorts only), legacy_owner_stake?, legacy_linked_stake?]
//   portfolio cohorts: linked = Percolator portfolio and archive = its canonical cumulative telemetry PDA;
//   capital cohorts: linked = subledger position.
// data: cohort(u8)
fn register_start(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let cohort = *data.first().ok_or(ProgramError::InvalidInstructionData)?;
    if cohort > COHORT_FUNDING_PAYER {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let owner = next_account_info(iter)?;
    let recipient = next_account_info(iter)?;
    let linked = next_account_info(iter)?;
    let stake_account = next_account_info(iter)?;
    let system = next_account_info(iter)?;
    let portfolio_archive = if matches!(cohort, COHORT_LP | COHORT_TRADER | COHORT_FUNDING_PAYER) {
        Some(next_account_info(iter)?)
    } else {
        None
    };

    // `owner` must SIGN: registering binds this stake's COIN recipient, a privileged act only the
    // rightful party may authorize. Without it, anyone could front-run the victim's (per-owner)
    // stake PDA naming themselves recipient, permanently denying the victim their share (finding GY).
    if !payer.is_signer || !owner.is_signer || config_account.owner != program_id {
        return Err(if config_account.owner != program_id {
            ProgramError::IllegalOwner
        } else {
            ProgramError::MissingRequiredSignature
        });
    }
    // The COIN recipient must be a real key (finding IK): a default-pubkey recipient is never legitimate,
    // and a crystallized stake bound to it can NEVER be sealed — distribution::append rejects a
    // default-pubkey entry, yet HD/HX completeness require every crystallized stake represented, so one
    // such (active) stake makes the seal permanently unsatisfiable = a single-stake DOS on any genesis.
    if *recipient.key == Pubkey::default() {
        return Err(ProgramError::InvalidArgument);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config.freeze_slot != 0 {
        return Err(ProgramError::InvalidAccountData); // denominators frozen — no new registrations
    }
    let family_seed = [stake_family(cohort)?];
    // Pre-epoch configs may already have an owner-only V0 or owner+linked V1 stake whose points
    // remain in a live denominator. A new family-scoped stake for the same economics would count
    // those points twice and both schemas would remain claimable. Require both canonical predecessor
    // addresses and reject an already-counted same-family witness before creating a current stake.
    // A zero-point predecessor may migrate, and merely donated lamports remain harmless.
    if config_account.data_len() == PRE_EPOCH_CONFIG_SIZE {
        let legacy_owner_stake = next_account_info(iter)?;
        let legacy_linked_stake = next_account_info(iter)?;
        if iter.next().is_some() {
            return Err(ProgramError::InvalidInstructionData);
        }
        let expected_owner_stake = Pubkey::find_program_address(
            &[b"rd_stake", config_account.key.as_ref(), owner.key.as_ref()],
            program_id,
        )
        .0;
        let expected_linked_stake = Pubkey::find_program_address(
            &[
                b"rd_stake",
                config_account.key.as_ref(),
                owner.key.as_ref(),
                linked.key.as_ref(),
            ],
            program_id,
        )
        .0;
        if *legacy_owner_stake.key != expected_owner_stake
            || *legacy_linked_stake.key != expected_linked_stake
        {
            return Err(ProgramError::InvalidSeeds);
        }
        for (legacy_stake, linked_seed) in [
            (legacy_owner_stake, false),
            (legacy_linked_stake, true),
        ] {
            if legacy_stake.data_len() == 0 {
                continue;
            }
            if legacy_stake.owner != program_id {
                return Err(ProgramError::IllegalOwner);
            }
            let predecessor = Stake::deserialize(&legacy_stake.try_borrow_data()?)?;
            if predecessor.config != *config_account.key
                || predecessor.owner != *owner.key
                || (linked_seed && predecessor.backing_ledger != *linked.key)
            {
                return Err(ProgramError::InvalidAccountData);
            }
            if predecessor.backing_ledger == *linked.key
                && stake_family(predecessor.cohort)? == family_seed[0]
                && predecessor.points != 0
            {
                return Err(ProgramError::AccountAlreadyInitialized);
            }
        }
    } else if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let now = Clock::get()?.slot;
    if config.config_kind == CONFIG_KIND_REWARD_EPOCH
        && (now < config.emission_start_slot || now >= config.emission_end_slot)
    {
        return Err(ProgramError::InvalidInstructionData);
    }
    // `snap` is the register-time counter snapshot: 0 for capital cohorts (insurance/backing),
    // and the relevant portfolio-flow counter for portfolio cohorts (delta measured at crystallize).
    let snap: u128 = match cohort {
        COHORT_INSURANCE | COHORT_BACKING => {
            // Capital cohort: `linked` is a subledger Position in this cohort's pool.
            if *linked.owner != config.subledger_program {
                return Err(ProgramError::IllegalOwner);
            }
            let data = linked.try_borrow_data()?;
            validate_subledger_position(&data)?;
            // Bind the position to its depositor (finding GY): only the rightful owner may register it.
            if pk(&data, SUB_POS_OWNER) != *owner.key {
                return Err(ProgramError::IllegalOwner);
            }
            // Scope to one immutable pool in this epoch's DAO-selected market set. Legacy genesis
            // configs have only the primary pool, so they traverse this same check.
            if !config.pool_allowed(cohort, &pk(&data, SUB_POS_POOL)) {
                return Err(ProgramError::IllegalOwner);
            }
            0
        }
        _ => {
            // Include cumulative terminal archives in the start snapshot. A portfolio address can be
            // dematerialized and reused; omitting prior generations here would count old flow twice.
            let totals = portfolio_totals(
                program_id,
                &config,
                owner.key,
                linked,
                portfolio_archive.ok_or(ProgramError::NotEnoughAccountKeys)?,
            )?;
            match cohort {
                COHORT_LP | COHORT_TRADER => residual_counter(
                    cohort,
                    totals.residual_received,
                    totals.residual_crystallized_loss,
                    totals.residual_spent_principal,
                ),
                _ => funding_payer_counter(
                    totals.funding_long_paid,
                    totals.funding_short_paid,
                ),
            }
        }
    };
    let start_slot = now;
    let (expected, bump) = Pubkey::find_program_address(
        &stake_seeds(config_account.key, owner.key, linked.key, &family_seed),
        program_id,
    );
    if *stake_account.key != expected || stake_account.data_len() != 0 {
        return Err(ProgramError::InvalidSeeds);
    }
    let bump_arr = [bump];
    let seeds: [&[u8]; 6] = [
        b"rd_stake",
        config_account.key.as_ref(),
        owner.key.as_ref(),
        linked.key.as_ref(),
        &family_seed,
        &bump_arr,
    ];
    create_pda(payer, stake_account, system, program_id, &seeds, STAKE_SIZE)?;
    Stake {
        config: *config_account.key,
        owner: *owner.key,
        backing_ledger: *linked.key,
        recipient: *recipient.key,
        residual_snap: snap,
        earnings_snap: 0,
        start_slot,
        points: 0,
        bump,
        cohort,
        eligible_accum: 0,
        claimed: false,
    }
    .serialize(&mut stake_account.try_borrow_mut_data()?);
    Ok(())
}

// crystallize accounts: [cranker(s), config(w), stake(w), backing_ledger,
//   portfolio_archive (portfolio cohorts only)]
fn crystallize(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let stake_account = next_account_info(iter)?;
    let backing_ledger = next_account_info(iter)?;

    if !cranker.is_signer || config_account.owner != program_id || stake_account.owner != program_id
    {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config.sealed != 0 || config.freeze_slot != 0 {
        return Err(ProgramError::InvalidAccountData); // sealed or frozen -> denominators are final
    }
    let now = Clock::get()?.slot;
    let post_emission_finalize = if config.config_kind == CONFIG_KIND_REWARD_EPOCH {
        if now < config.emission_start_slot {
            return Err(ProgramError::InvalidInstructionData);
        }
        now > config.emission_end_slot
    } else {
        false
    };
    let mut stake = Stake::deserialize(&stake_account.try_borrow_data()?)?;
    if stake.cohort > COHORT_FUNDING_PAYER {
        return Err(ProgramError::InvalidAccountData);
    }
    let portfolio_archive = if matches!(
        stake.cohort,
        COHORT_LP | COHORT_TRADER | COHORT_FUNDING_PAYER
    ) {
        Some(next_account_info(iter)?)
    } else {
        None
    };
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    // Reward counters stop accruing at emission_end. During the delayed freeze window, permit only
    // a reduce-only trader refresh: residual spent can consume a previously crystallized loss and
    // otherwise leave stale points in the frozen denominator. Capital forfeiture remains owner-timed,
    // and monotonic LP/funding counters need no post-period mutation.
    if post_emission_finalize && stake.cohort != COHORT_TRADER {
        return Err(ProgramError::InvalidInstructionData);
    }
    if stake.config != *config_account.key || stake.backing_ledger != *backing_ledger.key {
        return Err(ProgramError::InvalidAccountData);
    }
    let family_seed = [stake_family(stake.cohort)?];
    let (expected_stake, _) = Pubkey::find_program_address(
        &stake_seeds(
            config_account.key,
            &stake.owner,
            &stake.backing_ledger,
            &family_seed,
        ),
        program_id,
    );
    if *stake_account.key != expected_stake {
        return Err(ProgramError::InvalidSeeds);
    }
    // subtract-old/add-new keeps the cohort denominator authoritative as points are re-derived.
    match stake.cohort {
        COHORT_INSURANCE | COHORT_BACKING => {
            // Capital cohort: points = floor(log2(tenure)) * LIVE Position.principal (0 if exited).
            // Principal is a comparable base-unit amount across the DAO-selected pools; raw shares are
            // not comparable because every pool has its own loss/surplus-dependent share price.
            // The position clock resets on every top-up, so use the later of registration and position
            // start. This prevents dust registration from lending old tenure to late capital.
            // OWNER-GATED (finding KO, KM parity): crystallize OVERWRITES stake.points from live
            // principal NOW, and freeze then locks that value as the frozen denominator term - which the
            // claim-time min-cap can only ever LOWER, never raise. So a permissionless caller could
            // force-crystallize a victim after a partial withdrawal (withdrawn=false, principal reduced)
            // and `freeze` to lock the victim's COIN share permanently
            // low. A capital-cohort re-crystallize must therefore be authorized by the stake's owner.
            // (portfolio-flow cohorts stay permissionless).
            if cranker.key != &stake.owner {
                return Err(ProgramError::MissingRequiredSignature);
            }
            if *backing_ledger.owner != config.subledger_program {
                return Err(ProgramError::IllegalOwner);
            }
            let data = backing_ledger.try_borrow_data()?;
            let (principal, withdrawn) = read_subledger_principal(&data)?;
            let position_start_slot = read_subledger_start_slot(&data)?;
            let now = Clock::get()?.slot;
            let (live_principal, crystallized_slot) =
                match read_terminal_return_snapshot(&data)? {
                    Some((terminal_principal, Some(return_slot))) => {
                        (terminal_principal, core::cmp::min(now, return_slot))
                    }
                    // Historical terminal snapshots predate the return-slot field.
                    // Preserve their already-crystallized claim cap, but do not
                    // guess a tenure and inflate a newly crystallized reward.
                    Some((_terminal_principal, None)) => (0, now),
                    None => (capital_points(principal, withdrawn), now),
                };
            let effective_start = core::cmp::max(stake.start_slot, position_start_slot);
            let multiplier = floor_log2(crystallized_slot.saturating_sub(effective_start));
            let new_pts = multiplier.saturating_mul(live_principal);
            replace_cohort_points(
                config.cohort_points_mut(stake.cohort),
                stake.points,
                new_pts,
            )?;
            stake.points = new_pts;
            stake.earnings_snap = live_principal;
            stake.eligible_accum = crystallized_slot as u128;
        }
        COHORT_LP | COHORT_TRADER => {
            // Residual cohorts: points = TIME-WEIGHTED delta of LP residual_received or trader
            // crystallized_loss - spent since register.
            let totals = portfolio_totals(
                program_id,
                &config,
                &stake.owner,
                backing_ledger,
                portfolio_archive.ok_or(ProgramError::NotEnoughAccountKeys)?,
            )?;
            let spent = totals.residual_spent_principal;
            let counter = residual_counter(
                stake.cohort,
                totals.residual_received,
                totals.residual_crystallized_loss,
                spent,
            );
            let net_delta = counter.saturating_sub(stake.residual_snap);
            let (new_pts, new_net) = if post_emission_finalize {
                let cap_net =
                    trader_live_cap(stake.earnings_snap, stake.eligible_accum, net_delta, spent);
                (
                    cap_residual_points(stake.points, stake.earnings_snap, cap_net)?,
                    cap_net,
                )
            } else {
                let tenure = now.saturating_sub(stake.start_slot);
                (
                    floor_log2(tenure)
                        .checked_mul(net_delta)
                        .ok_or(ProgramError::ArithmeticOverflow)?,
                    net_delta,
                )
            };
            replace_cohort_points(
                config.cohort_points_mut(stake.cohort),
                stake.points,
                new_pts,
            )?;
            stake.points = new_pts;
            stake.earnings_snap = new_net;
            stake.eligible_accum = if stake.cohort == COHORT_TRADER {
                spent
            } else {
                0
            };
        }
        _ => {
            // Funding-payer cohort: points = raw delta of paid funding since register. No age multiplier:
            // funding accumulators already represent settled payment volume, and late payments should not
            // inherit early registration tenure.
            let totals = portfolio_totals(
                program_id,
                &config,
                &stake.owner,
                backing_ledger,
                portfolio_archive.ok_or(ProgramError::NotEnoughAccountKeys)?,
            )?;
            let counter = funding_payer_counter(
                totals.funding_long_paid,
                totals.funding_short_paid,
            );
            let net_delta = counter.saturating_sub(stake.residual_snap);
            let new_pts = net_delta;
            replace_cohort_points(
                config.cohort_points_mut(stake.cohort),
                stake.points,
                new_pts,
            )?;
            stake.points = new_pts;
            stake.earnings_snap = net_delta;
            stake.eligible_accum = 0;
        }
    }

    stake.serialize(&mut stake_account.try_borrow_mut_data()?);
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// freeze accounts: [cranker(s), config(w), coin_mint, vault]
//
// Permissionless. After emission_end, this is the one-shot transition from the accrual phase
// (register/crystallize) to the self-service claim phase. It (1) snapshots the cohort denominators
// (total_points, insurance_total_points) and stamps freeze_slot, after which register/crystallize are
// closed so the denominators are final; and (2) BINDS + verifies the COIN vault claims pay from: it
// must be the initialized token account owned by this config PDA, with the coin_mint carrying NO
// mint or freeze authority. Fixed mode requires the whole mint; reward-epoch mode snapshots the
// pre-bound vault balance. Double-freeze is rejected so neither supply nor denominators can move.
fn freeze(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    if !cranker.is_signer || config_account.owner != program_id {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config.freeze_slot != 0 {
        return Err(ProgramError::InvalidAccountData); // already frozen — snapshot + vault are immutable
    }
    let now = Clock::get()?.slot;
    let freeze_cutoff = config
        .emission_end_slot
        .checked_add(config.finalize_window)
        .ok_or(ProgramError::InvalidInstructionData)?;
    if now < freeze_cutoff {
        return Err(ProgramError::InvalidInstructionData); // emission + finalize window still open
    }
    // GX: the COIN is a fixed pool — no mint authority (can't inflate) and no freeze authority (can't
    // freeze a claimer's account). EZ: the bound vault is rd_config-owned and holds the WHOLE supply.
    if *coin_mint.key != config.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    if config.config_kind == CONFIG_KIND_REWARD_EPOCH && *vault.key != config.vault {
        return Err(ProgramError::InvalidAccountData);
    }
    // Require SPL Token ownership BEFORE unpacking (parity with distribution::init_config:342): Pack::unpack
    // verifies bytes + length but NOT the owning program, so a NON-SPL account with token/mint-shaped bytes
    // would otherwise pass every field check below. freeze is permissionless + one-shot and BINDS config.vault,
    // so without this a griefer could front-run with a fake (non-SPL) token-shaped vault — owner field rd_config,
    // mint coin_mint, amount supply — permanently binding it; every claim's spl transfer from that source then
    // fails and the whole residual distribution is bricked (finding: rd freeze missing the SPL-owner guard).
    if coin_mint.owner != &spl_token::ID || vault.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let mint = spl_token::state::Mint::unpack(&coin_mint.try_borrow_data()?)?;
    if mint.mint_authority.is_some() || mint.freeze_authority.is_some() {
        return Err(ProgramError::InvalidAccountData);
    }
    let v = spl_token::state::Account::unpack(&vault.try_borrow_data()?)?;
    // The vault must be initialized, solely PDA-owned, non-native, and free of delegate/close paths.
    // AccountState::Initialized is load-bearing: an account frozen before freeze-authority revocation
    // stays frozen forever and would otherwise consume this one-shot transition while bricking claims.
    if v.state != spl_token::state::AccountState::Initialized
        || v.owner != *config_account.key
        || v.mint != config.coin_mint
        || v.delegate.is_some()
        || v.delegated_amount != 0
        || v.close_authority.is_some()
        || v.is_native.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    match config.reward_supply_mode {
        REWARD_SUPPLY_FIXED => {
            if config.total_supply == 0
                || mint.supply != config.total_supply
                || v.amount < config.total_supply
            {
                return Err(ProgramError::InvalidAccountData);
            }
        }
        REWARD_SUPPLY_VAULT_BALANCE => {
            if config.config_kind != CONFIG_KIND_REWARD_EPOCH || v.amount == 0 {
                return Err(ProgramError::InvalidAccountData);
            }
            config.total_supply = v.amount;
        }
        _ => return Err(ProgramError::InvalidAccountData),
    }
    config.vault = *vault.key;
    // Snapshot all cohort denominators.
    config.frozen_insurance_total_points = config.insurance_total_points;
    config.frozen_total_points = config.total_points; // BACKING
    config.frozen_lp_total_points = config.lp_total_points;
    config.frozen_trader_total_points = config.trader_total_points;
    config.frozen_funding_payer_total_points = config.funding_payer_total_points;
    config.reserved_frozen_funding_payer_total_points = config.reserved_funding_payer_total_points;
    config.freeze_slot = now;
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// claim accounts: [cranker(s), config, stake(w), vault(w), recipient_ata(w), token_program]
//   insurance/backing cohorts append one more: the subledger position (for the live HE cap).
//   LP/trader cohorts append two more: the Percolator portfolio and canonical cumulative archive.
//   A pre-archive direct Percolator maintenance close retains the historical frozen fallback.
//
// PERMISSIONLESS self-service claim (replaces the cranker-assembled seal for the portfolio
// cohort). Pays the stake's OWN deterministic share —
// `cohort_supply * stake.points / frozen_total_points` — to the stake's BOUND recipient, then marks
// it claimed. Each backer pulls their own slice; nobody assembles a global list, so there is no
// one-tx completeness seal (IG dissolved) and no cranker can omit or redirect a backer (the recipient
// is bound at register, finding GY, and re-checked here). Sum of all residual claims <= residual_supply
// (floor math), so the vault can never be over-drawn. Funding-payer claims use the crystallized,
// now-frozen `stake.points` from monotonic paid counters, so there is no live-position dependency and
// no HE concern and deliberately do not require the Percolator portfolio at claim time; users may
// close or dematerialize flat portfolios after crystallize/freeze. The capital and residual cohorts
// (live, HE-capped) are handled separately.
fn claim(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let stake_account = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let recipient_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if !cranker.is_signer || config_account.owner != program_id || stake_account.owner != program_id
    {
        return Err(ProgramError::MissingRequiredSignature);
    }
    // Pin token_program to the real SPL token program (defense-in-depth, matching distribution:619). A
    // substituted token_program is ALREADY rejected by spl_token::instruction::transfer's internal
    // check_program_account (propagated by `?` below, BEFORE any foreign program is invoked), so the
    // "no-op program nullifies a claim" grief is blocked regardless; this explicit guard makes the
    // invariant local + survives a future refactor to a hand-built transfer instruction (finding KE).
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config.freeze_slot == 0 {
        return Err(ProgramError::InvalidAccountData); // not frozen -> denominators not final
    }
    if *vault.key != config.vault {
        return Err(ProgramError::InvalidAccountData); // only the bound funded vault — no decoy
    }
    let mut stake = Stake::deserialize(&stake_account.try_borrow_data()?)?;
    if stake.cohort > COHORT_FUNDING_PAYER {
        return Err(ProgramError::InvalidAccountData);
    }
    if stake.config != *config_account.key {
        return Err(ProgramError::InvalidAccountData);
    }
    let family_seed = [stake_family(stake.cohort)?];
    let (expected_stake, expected_bump) = Pubkey::find_program_address(
        &stake_seeds(
            config_account.key,
            &stake.owner,
            &stake.backing_ledger,
            &family_seed,
        ),
        program_id,
    );
    if *stake_account.key != expected_stake || stake.bump != expected_bump {
        // Claim-only compatibility for the two predecessor PDA schemas. They can
        // exist only with the exact pre-epoch config; register/crystallize retain
        // the family-scoped check above, so old stakes cannot resume point accrual.
        if config.config_kind != CONFIG_KIND_LEGACY
            || config_account.data_len() != PRE_EPOCH_CONFIG_SIZE
        {
            return Err(ProgramError::InvalidSeeds);
        }
        let (linked_key, linked_bump) = Pubkey::find_program_address(
            &[
                b"rd_stake",
                config_account.key.as_ref(),
                stake.owner.as_ref(),
                stake.backing_ledger.as_ref(),
            ],
            program_id,
        );
        let linked_match = *stake_account.key == linked_key && stake.bump == linked_bump;
        let (owner_key, owner_bump) = Pubkey::find_program_address(
            &[
                b"rd_stake",
                config_account.key.as_ref(),
                stake.owner.as_ref(),
            ],
            program_id,
        );
        let owner_match = *stake_account.key == owner_key && stake.bump == owner_bump;
        if !linked_match && !owner_match {
            return Err(ProgramError::InvalidSeeds);
        }
    }
    if stake.claimed {
        return Err(ProgramError::InvalidAccountData); // double-claim
    }
    // Capital cohorts (insurance/backing) cap the payout by LIVE principal at claim time, so the claim
    // SLOT is value-relevant. claim is otherwise permissionless; if a third party could trigger a
    // capital claim they could force it during a transient low-principal moment after a partial
    // insurance-withdraw, which leaves the Position withdrawn=false but principal reduced - and the
    // irreversible claimed-flag would lock in the reduced (or zero) payout, stranding the remainder
    // (finding KM). So a capital claim must be authorized by the stake's OWN owner (the depositor who
    // controls the position and bears the soft-veto timing). Portfolio-flow cohorts pay frozen points from
    // account counters, so their claim slot is irrelevant and stays permissionless even if Percolator
    // has already dematerialized the witness; payout still goes only to the bound recipient.
    if matches!(stake.cohort, COHORT_INSURANCE | COHORT_BACKING) && cranker.key != &stake.owner {
        return Err(ProgramError::MissingRequiredSignature);
    }
    // The COIN must land in the bound recipient's own account (finding GY: no cranker redirect).
    // Portfolio-flow claims are permissionless, so owner equality is not sufficient: a cranker
    // could select a pre-existing recipient-owned account delegated to itself, force the payout
    // there, and spend it immediately. Require sole recipient control on those public claims.
    if recipient_ata.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let ra = spl_token::state::Account::unpack(&recipient_ata.try_borrow_data()?)?;
    if ra.state != spl_token::state::AccountState::Initialized
        || ra.owner != stake.recipient
        || ra.mint != config.coin_mint
        || (matches!(
            stake.cohort,
            COHORT_LP | COHORT_TRADER | COHORT_FUNDING_PAYER
        ) && (ra.delegate.is_some() || ra.delegated_amount != 0))
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let cohort_supply = config.cohort_supply(stake.cohort);
    let frozen_denom = config.frozen_cohort_points(stake.cohort);
    if stake.points > frozen_denom {
        return Err(ProgramError::InvalidAccountData);
    }
    let amount = match stake.cohort {
        COHORT_INSURANCE | COHORT_BACKING => {
            // Capital cohort: read LIVE Position principal and its resettable start clock atomically.
            // Less principal lowers the payout; a full exit pays zero. A later top-up cannot restore frozen
            // tenure because the live clock is measured at the stored crystallization slot.
            let position = next_account_info(iter)?;
            if iter.next().is_some() {
                return Err(ProgramError::InvalidInstructionData);
            }
            if *position.key != stake.backing_ledger || *position.owner != config.subledger_program
            {
                return Err(ProgramError::InvalidAccountData);
            }
            let data = position.try_borrow_data()?;
            let (principal, withdrawn) = read_subledger_principal(&data)?;
            // An owner-directed exit forfeits rewards, but a permissionless terminal
            // cranker cannot choose that outcome for the owner. Subledger records the
            // principal remaining immediately before that fixed full return. This is
            // still capped by the crystallized principal below, and any earlier
            // voluntary partial withdrawal remains excluded.
            let live_principal = read_terminal_return_snapshot(&data)?
                .map(|(principal, _return_slot)| principal)
                .unwrap_or_else(|| capital_points(principal, withdrawn));
            let position_start_slot = read_subledger_start_slot(&data)?;
            let crystallized_slot = u64::try_from(stake.eligible_accum)
                .map_err(|_| ProgramError::InvalidAccountData)?;
            let effective_start = core::cmp::max(stake.start_slot, position_start_slot);
            let live_multiplier = floor_log2(crystallized_slot.saturating_sub(effective_start));
            let capped_principal = core::cmp::min(stake.earnings_snap, live_principal);
            let live_pts = live_multiplier.saturating_mul(capped_principal);
            let pts = if stake.points < live_pts {
                stake.points
            } else {
                live_pts
            };
            points_to_amount(cohort_supply, pts, frozen_denom)
        }
        COHORT_LP | COHORT_TRADER => {
            // Residual cohorts: live-cap the frozen points against a post-crystallize net drop.
            let portfolio = next_account_info(iter)?;
            let portfolio_archive = next_account_info(iter)?;
            if iter.next().is_some() {
                return Err(ProgramError::InvalidInstructionData);
            }
            if *portfolio.key != stake.backing_ledger {
                return Err(ProgramError::InvalidAccountData);
            }
            let dematerialized = portfolio.data_len() == 0
                && (*portfolio.owner == solana_program::system_program::ID
                    || *portfolio.owner == config.percolator_program);
            if dematerialized && portfolio_archive.data_len() == 0 {
                if *portfolio_archive.owner != solana_program::system_program::ID
                    || !archive_key_matches_allowed_market(
                        &config,
                        portfolio_archive.key,
                        &stake.owner,
                        portfolio.key,
                    )
                {
                    return Err(ProgramError::InvalidSeeds);
                }
                // Percolator can dematerialize an empty portfolio through maintenance-fee or
                // old owner-directed paths that predate the controller archive. If counters were
                // crystallized and frozen before that close, the bound recipient and denominator
                // remain sufficient. Controller cleanup now always supplies an archive instead.
                points_to_amount(cohort_supply, stake.points, frozen_denom)
            } else {
                let totals = portfolio_totals(
                    program_id,
                    &config,
                    &stake.owner,
                    portfolio,
                    portfolio_archive,
                )?;
                let spent = totals.residual_spent_principal;
                let live_net = residual_counter(
                    stake.cohort,
                    totals.residual_received,
                    totals.residual_crystallized_loss,
                    spent,
                )
                .saturating_sub(stake.residual_snap);
                let frozen_net = stake.earnings_snap; // net_delta captured at the last crystallize
                let cap_net = if stake.cohort == COHORT_TRADER {
                    trader_live_cap(frozen_net, stake.eligible_accum, live_net, spent)
                } else {
                    live_net
                };
                let pts = cap_residual_points(stake.points, frozen_net, cap_net)?;
                points_to_amount(cohort_supply, pts, frozen_denom)
            }
        }
        _ => {
            // Funding-payer cohort: crystallize authenticated the bound Percolator portfolio and froze points
            // from monotonic paid-funding counters. Do not require the portfolio again at claim time; a flat
            // portfolio may be closed or terminal-cleaned before the owner claims COIN, and the frozen numerator
            // plus frozen denominator already conserve the cohort.
            if iter.next().is_some() {
                return Err(ProgramError::InvalidInstructionData);
            }
            points_to_amount(cohort_supply, stake.points, frozen_denom)
        }
    };
    // ANTI-WASH FEE (finding NZ): portfolio-flow cohorts are farmable by synthetic/wash flow on allow-listed
    // markets, so they pay a fee retained in the vault. Capital cohorts are at risk and pay no fee.
    let fee = if matches!(
        stake.cohort,
        COHORT_LP | COHORT_TRADER | COHORT_FUNDING_PAYER
    ) {
        // CEIL the fee, not floor: a flooring fee rounds to 0 on dust claims (amount*bps < BPS_DENOMINATOR),
        // letting a Sybil farmer FRAGMENT one farm into many dust stakes that EACH pay 0 fee and dodge the
        // anti-wash skim entirely (the fee is the sole economic bound on the delta-neutral cross-margin wash).
        // Ceiling makes every nonzero funding-payer claim pay >= 1 atom, so fragmentation can never reduce the
        // effective fee below the intended rate. fee <= amount holds (bps <= BPS_DENOMINATOR):
        // ceil(amount*bps/DEN) <= amount, so the payout subtraction never underflows.
        ((amount as u128) * (config.fee_support_bps as u128)).div_ceil(BPS_DENOMINATOR as u128)
            as u64
    } else {
        0
    };
    let payout = amount - fee; // fee <= amount (bps <= 10000), retained in the rd vault
                               // Mark claimed before paying (the whole tx reverts on a transfer failure, so this is atomic).
    stake.claimed = true;
    stake.serialize(&mut stake_account.try_borrow_mut_data()?);
    if payout > 0 {
        let bump_arr = [config.bump];
        let transfer = spl_token::instruction::transfer(
            token_program.key,
            vault.key,
            recipient_ata.key,
            config_account.key,
            &[],
            payout,
        )?;
        let infos = [
            vault.clone(),
            recipient_ata.clone(),
            config_account.clone(),
            token_program.clone(),
        ];
        match config.config_kind {
            CONFIG_KIND_LEGACY => {
                let signer_seeds: [&[u8]; 3] = [b"rd_config", config.coin_mint.as_ref(), &bump_arr];
                invoke_signed(&transfer, &infos, &[&signer_seeds])?;
            }
            CONFIG_KIND_REWARD_EPOCH => {
                let epoch_bytes = config.epoch_id.to_le_bytes();
                let signer_seeds: [&[u8]; 5] = [
                    b"rd_epoch",
                    config.epoch_authority.as_ref(),
                    config.coin_mint.as_ref(),
                    &epoch_bytes,
                    &bump_arr,
                ];
                let expected = Pubkey::create_program_address(&signer_seeds, program_id)
                    .map_err(|_| ProgramError::InvalidSeeds)?;
                if expected != *config_account.key {
                    return Err(ProgramError::InvalidSeeds);
                }
                invoke_signed(&transfer, &infos, &[&signer_seeds])?;
            }
            _ => return Err(ProgramError::InvalidAccountData),
        }
    }
    Ok(())
}

fn take_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    let b = data.get(..8).ok_or(ProgramError::InvalidInstructionData)?;
    *data = &data[8..];
    Ok(u64::from_le_bytes(b.try_into().unwrap()))
}
fn take_u16(data: &mut &[u8]) -> Result<u16, ProgramError> {
    let b = data.get(..2).ok_or(ProgramError::InvalidInstructionData)?;
    *data = &data[2..];
    Ok(u16::from_le_bytes(b.try_into().unwrap()))
}
fn take_pubkey(data: &mut &[u8]) -> Result<Pubkey, ProgramError> {
    let b = data.get(..32).ok_or(ProgramError::InvalidInstructionData)?;
    *data = &data[32..];
    Ok(Pubkey::new_from_array(b.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_is_pro_rata_and_never_over_allocates() {
        assert_eq!(points_to_amount(1_000_000, 30, 100), 300_000);
        assert_eq!(points_to_amount(1_000_000, 70, 100), 700_000);
        assert!(
            points_to_amount(1_000_000, 30, 100) + points_to_amount(1_000_000, 70, 100)
                <= 1_000_000
        );
        assert_eq!(points_to_amount(1_000_000, 1, 0), 0);
    }

    #[test]
    fn reads_live_subledger_capital_offsets() {
        let mut d = [0u8; 120];
        d[..8].copy_from_slice(&SUB_POSITION_DISC);
        d[72..80].copy_from_slice(&555u64.to_le_bytes()); // principal
        d[88] = 1; // withdrawn
        d[89..97].copy_from_slice(&4242u64.to_le_bytes()); // resettable deposit clock
        d[104..120].copy_from_slice(&777u128.to_le_bytes()); // unrelated pool-local shares
        let (principal, w) = read_subledger_principal(&d).unwrap();
        assert_eq!(principal, 555);
        assert!(w);
        assert_eq!(read_subledger_start_slot(&d).unwrap(), 4242);
        assert_eq!(capital_points(555, true), 0, "withdrawn -> forfeit");
        assert_eq!(capital_points(555, false), 555, "live -> principal");
    }

    #[test]
    fn portfolio_identity_rejects_short_data_without_panicking() {
        let cfg = Config {
            coin_mint: Pubkey::new_unique(),
            distribution_program: Pubkey::new_unique(),
            distribution_config: Pubkey::new_unique(),
            percolator_program: Pubkey::new_unique(),
            total_supply: 1,
            fee_support_bps: 0,
            emission_end_slot: 0,
            total_points: 0,
            sealed: 0,
            bump: 0,
            insurance_bps: 0,
            insurance_total_points: 0,
            subledger_program: Pubkey::new_unique(),
            subledger_pool: Pubkey::new_unique(),
            market_group: Pubkey::new_unique(),
            frozen_total_points: 0,
            frozen_insurance_total_points: 0,
            freeze_slot: 0,
            vault: Pubkey::default(),
            finalize_window: 0,
            backing_pool: Pubkey::new_unique(),
            backing_bps: 0,
            lp_bps: 0,
            lp_total_points: 0,
            trader_total_points: 0,
            frozen_lp_total_points: 0,
            frozen_trader_total_points: 0,
            extra_market_count: 0,
            extra_markets: Vec::new(),
            funding_payer_bps: 0,
            reserved_funding_payer_bps: 0,
            funding_payer_total_points: 0,
            reserved_funding_payer_total_points: 0,
            frozen_funding_payer_total_points: 0,
            reserved_frozen_funding_payer_total_points: 0,
            epoch_authority: Pubkey::default(),
            epoch_id: 0,
            emission_start_slot: 0,
            reward_supply_mode: REWARD_SUPPLY_FIXED,
            config_kind: CONFIG_KIND_LEGACY,
            extra_insurance_pools: Vec::new(),
            extra_backing_pools: Vec::new(),
        };
        let owner = Pubkey::new_unique();
        let result =
            std::panic::catch_unwind(|| validate_portfolio_identity(&cfg, &[0u8; 32], &owner));
        assert!(
            result.is_ok(),
            "short portfolio data must return an error, not panic"
        );
        assert_eq!(result.unwrap(), Err(ProgramError::AccountDataTooSmall));
    }
}
