//! Deterministic, points-based COIN distribution decider.
//!
//! **Branch `risidual_genesis_never_push_upstream` — do NOT push upstream.**
//!
//! Drop-in alternative to `genesis-vote` behind the `distribution` program's
//! pluggable-decider seam (see distribution/src/lib.rs "Decider seam"). The winning
//! COIN allocation is computed **deterministically** from residual-backing points,
//! so there is nothing for a late whale to capture: every backer's share is fixed
//! by the risk it actually bore.
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

const CONFIG_DISC: [u8; 8] = *b"RDCONFG1";
const STAKE_DISC: [u8; 8] = *b"RDSTAKE1";
// Up to this many ADDITIONAL allow-listed markets beyond the primary `market_group` (finding IL+): the
// portfolio-flow cohorts read percolator portfolio counters that an attacker can manufacture if they control the
// market's oracle, so a portfolio is only countable if its market is on this orchestrator-vetted allow-list.
// The creator stands up N trusted-Pyth markets while holding the market-auth key locally, vets them, then
// transfers that key to the PDA that rotates it to the DAO — so the allow-listed markets cannot later be
// repointed at an attacker oracle. See DESIGN.md "Market allow-list".
const MAX_EXTRA_MARKETS: usize = 9; // 10 total allow-listed markets (market_group + 9 extras)
const CONFIG_FUNDING_TAIL_OFF: usize = 466 + 1 + MAX_EXTRA_MARKETS * 32;
const CONFIG_SIZE: usize = CONFIG_FUNDING_TAIL_OFF + 68;
const STAKE_SIZE: usize = 211; // +1 claimed flag (self-service)
                               // Share cohorts + portfolio-flow cohorts. Insurance/backing reward SHARE VALUE; LP/trader reward residual
                               // counters; optional funding-payer cohort rewards the sum of Percolator funding-paid counters. See tests/offsets.rs.
const COHORT_INSURANCE: u8 = 0;
const COHORT_BACKING: u8 = 1;
const COHORT_LP: u8 = 2;
const COHORT_TRADER: u8 = 3;
const COHORT_FUNDING_PAYER: u8 = 4;

const IX_INIT: u8 = 0;
const IX_REGISTER_START: u8 = 1;
const IX_CRYSTALLIZE: u8 = 2;
// tag 3 (legacy cranker IX_SEAL) RETIRED — superseded by the self-service freeze+claim path below.
// Self-service path (replacing the cranker seal). After emission_end, IX_FREEZE snapshots the
// cohort denominators and closes register/crystallize; backers then finalize/claim their own share.
const IX_FREEZE: u8 = 4;
const IX_CLAIM: u8 = 5;

// ===========================================================================
// Deterministic, gaming-resistant point math  (pure — unit-tested below)
// ===========================================================================

/// Deterministic pro-rata split; floor rounding never over-allocates the fixed pool.
///
/// Overflow defense: `total_supply` (u64) * `points_i` (u128) can exceed u128 when `points_i` is large
/// (`points_i = floor_log2(tenure) * net_delta` for residual cohorts), so we `saturating_mul`
/// rather than use an unchecked `*`
/// (which would PANIC = brick every claim in the cohort) or a wrapping `*` (which would DRAIN). Because
/// `points_i <= total_points` for any real stake, the result is ALWAYS `<= total_supply`, so this never
/// overpays/over-allocates the fixed pool regardless of saturation. Saturation is itself unreachable for
/// realistic inputs (`net_delta` is funding paid, bounded by the market's collateral, far below
/// the u64*u128 product limit); were it ever reached the claimant would UNDERpay and the remainder stays
/// LOCKED in the vault (conservation holds — never a drain). Do NOT replace the saturating_mul with `*`;
/// switch to u256 only if exactness past the (unreachable) saturation point is ever required.
pub fn points_to_amount(total_supply: u64, points_i: u128, total_points: u128) -> u64 {
    if total_points == 0 {
        return 0;
    }
    ((total_supply as u128).saturating_mul(points_i) / total_points) as u64
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

fn read_pubkey(data: &[u8], off: usize) -> Result<Pubkey, ProgramError> {
    let b = data
        .get(off..off + 32)
        .ok_or(ProgramError::AccountDataTooSmall)?;
    Ok(Pubkey::new_from_array(b.try_into().unwrap()))
}

// ===========================================================================
// Insurance cohort (the SOFT VETO half) — points read LIVE from the subledger
// position, so an exit (principal -> 0, withdrawn) AUTO-FORFEITS the COIN share.
// ===========================================================================
// subledger Position offsets (stable across the share-model change — appended
// fields only): principal u64@72, withdrawn u8@88, start_slot u64@89.
// Subledger Position offsets. PINNED against the subledger's exported POS_* consts by
// tests/offsets.rs (finding HF: a wrong owner offset here slipped past mocked tests).
pub const SUB_POS_POOL: usize = 8; // Position.pool @ 8 (real layout: disc@0, pool@8..40, owner@40..72).
pub const SUB_POS_OWNER: usize = 40; // Position.owner @ 40. The depositor owed this position's COIN.
pub const SUB_POS_WITHDRAWN: usize = 88;
// Position.shares (POLICY_WITH_SURPLUS) @104 — the SHARE-VALUE points source for the insurance AND
// backing cohorts. Within one pool the share price (balance/total_shares) is common, so pro-rata by
// share value == pro-rata by shares; shares also encode the fee/time weighting (an earlier depositor
// holds more shares per dollar) and give the soft-veto for free (exit redeems shares -> 0 -> forfeit).
pub const SUB_POS_SHARES: usize = 104;

/// (shares, withdrawn) from a live subledger Position — the SHARE-VALUE points for the insurance &
/// backing cohorts. A withdrawn (or zero-share) position yields 0 (soft veto): an exiter redeemed its
/// shares, forfeiting its COIN. Read LIVE at claim so a partial redeem can't over-claim.
pub fn read_subledger_shares(data: &[u8]) -> Result<(u128, bool), ProgramError> {
    let shares = read_u128(data, SUB_POS_SHARES)?;
    let withdrawn = *data
        .get(SUB_POS_WITHDRAWN)
        .ok_or(ProgramError::AccountDataTooSmall)?
        == 1;
    Ok((shares, withdrawn))
}

/// Share-value points: just the live shares (0 if exited). Pro-rata across the cohort's pool.
pub fn share_value_points(shares: u128, withdrawn: bool) -> u128 {
    if withdrawn {
        0
    } else {
        shares
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

fn validate_portfolio_identity(config: &Config, data: &[u8], owner: &Pubkey) -> ProgramResult {
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
    // Slots AFTER emission_end during which backers do their final crystallize before the denominators
    // lock. freeze is rejected until `emission_end + finalize_window`. Since freeze is PERMISSIONLESS,
    // a zero window would let anyone freeze the instant emission ends and forfeit slower backers' still
    // un-crystallized points; the orchestrator sets ~1 week here (the "finalize your points" window).
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
}
impl Config {
    fn deserialize(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() < CONFIG_SIZE || d[..8] != CONFIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Config {
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
        })
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

struct Stake {
    config: Pubkey,
    owner: Pubkey,
    backing_ledger: Pubkey,
    recipient: Pubkey,
    residual_snap: u128,
    // LOAD-BEARING (anti-wash live-cap): crystallize stores the realized `net_delta` here; claim reads it
    // as `frozen_net` and scales the payout by min(1, live_net/frozen_net) so a recovered loss (live_net <
    // frozen_net) pays proportionally less — closing the stale-points bypass of net-by-spent. Repurposed
    // from the superseded fee-cap design (see eligible_accum). MUST be preserved across crystallize/freeze.
    earnings_snap: u128,
    start_slot: u64,
    points: u128,
    bump: u8,
    // `backing_ledger` is the linked subledger position for insurance/backing, or the linked Percolator
    // portfolio for LP/trader/funding-payer cohorts.
    cohort: u8,
    // Retired scratch field for the old residual-spent cap. Held at 0 for the funding-payer cohort; kept for
    // serialized layout stability.
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
    if !data.is_empty() || !payer.is_signer || total_supply == 0 {
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
    // Bind distribution_config to the canonical PDA(["dist_config", coin_mint, rd_config]) under the
    // distribution program (finding HC; parity with genesis-vote finding R). rd_config (= `expected`)
    // is the distribution authority, so the ONLY config rd can ever seal is the one at this PDA.
    // Without this, a front-runner could squat this canonical (per-coin_mint) rd_config with a foreign
    // distribution_config; since rd_config can't be re-initialized, seal would forever target the
    // foreign config and the real COIN-holding distribution could never be sealed -> DOS.
    let (expected_dist, _) = Pubkey::find_program_address(
        &[b"dist_config", coin_mint.key.as_ref(), expected.as_ref()],
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
    }
    .serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// register_start accounts: [payer(s,w), config, owner, recipient, linked, stake(pda,w), system]
//   residual:  linked = percolator backing ledger; insurance: linked = subledger position.
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
    let now = Clock::get()?.slot;
    // `snap` is the register-time counter snapshot: 0 for the share-value cohorts (insurance/backing),
    // and the relevant portfolio-flow counter for portfolio cohorts (delta measured at crystallize).
    let snap: u128 = match cohort {
        COHORT_INSURANCE | COHORT_BACKING => {
            // Share-value cohort: `linked` is a subledger Position in this cohort's pool.
            if *linked.owner != config.subledger_program {
                return Err(ProgramError::IllegalOwner);
            }
            let data = linked.try_borrow_data()?;
            // Bind the position to its depositor (finding GY): only the rightful owner may register it.
            if pk(&data, SUB_POS_OWNER) != *owner.key {
                return Err(ProgramError::IllegalOwner);
            }
            // Scope to THIS genesis's pool (finding HG): insurance -> subledger_pool, backing -> backing_pool.
            let scope_pool = if cohort == COHORT_INSURANCE {
                config.subledger_pool
            } else {
                config.backing_pool
            };
            if pk(&data, SUB_POS_POOL) != scope_pool {
                return Err(ProgramError::IllegalOwner);
            }
            0
        }
        _ => {
            // Portfolio-flow cohort: `linked` is a Percolator PortfolioAccount.
            if *linked.owner != config.percolator_program {
                return Err(ProgramError::IllegalOwner); // counters must be percolator-authenticated
            }
            let data = linked.try_borrow_data()?;
            // Bind the portfolio to its owner and an allow-listed market (findings GY/IL+). Re-checked at
            // crystallize because Percolator can dematerialize and later reinitialize the same account key.
            validate_portfolio_identity(&config, &data, owner.key)?;
            match cohort {
                COHORT_LP | COHORT_TRADER => {
                    let (received, crystallized, spent) = read_portfolio_residual(&data)?;
                    residual_counter(cohort, received, crystallized, spent)
                }
                _ => {
                    let (long_paid, _, short_paid, _) = read_portfolio_funding_flow(&data)?;
                    funding_payer_counter(long_paid, short_paid)
                }
            }
        }
    };
    let start_slot = now;
    let family_seed = [stake_family(cohort)?];
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

// crystallize accounts: [cranker(s), config(w), stake(w), backing_ledger]
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
    let mut stake = Stake::deserialize(&stake_account.try_borrow_data()?)?;
    if stake.cohort > COHORT_FUNDING_PAYER {
        return Err(ProgramError::InvalidAccountData);
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
            // Share-value cohort: points = LIVE Position.shares (0 if exited — soft veto). The share
            // price is common within the pool, so shares == pro-rata share value, fee-weighted.
            // OWNER-GATED (finding KO, KM parity): crystallize OVERWRITES stake.points from the live
            // shares NOW, and freeze then locks that value as the frozen denominator term — which the
            // claim-time min-cap can only ever LOWER, never raise. So a permissionless caller could
            // force-crystallize a victim at a transient low-share moment (mid partial-withdraw:
            // withdrawn=false, shares reduced) and `freeze` to lock the victim's COIN share permanently
            // low. A share-value re-crystallize must therefore be authorized by the stake's own owner.
            // (portfolio-flow cohorts stay permissionless).
            if cranker.key != &stake.owner {
                return Err(ProgramError::MissingRequiredSignature);
            }
            if *backing_ledger.owner != config.subledger_program {
                return Err(ProgramError::IllegalOwner);
            }
            let (shares, withdrawn) = read_subledger_shares(&backing_ledger.try_borrow_data()?)?;
            let new_pts = share_value_points(shares, withdrawn);
            let slot = config.cohort_points_mut(stake.cohort);
            *slot = slot.saturating_sub(stake.points).saturating_add(new_pts);
            stake.points = new_pts;
        }
        COHORT_LP | COHORT_TRADER => {
            // Residual cohorts: points = TIME-WEIGHTED delta of LP residual_received or trader
            // crystallized_loss - spent since register.
            if *backing_ledger.owner != config.percolator_program {
                return Err(ProgramError::IllegalOwner);
            }
            let data = backing_ledger.try_borrow_data()?;
            validate_portfolio_identity(&config, &data, &stake.owner)?;
            let (received, crystallized, spent) = read_portfolio_residual(&data)?;
            let counter = residual_counter(stake.cohort, received, crystallized, spent);
            let net_delta = counter.saturating_sub(stake.residual_snap);
            let tenure = Clock::get()?.slot.saturating_sub(stake.start_slot);
            let new_pts = floor_log2(tenure).saturating_mul(net_delta);
            let slot = config.cohort_points_mut(stake.cohort);
            *slot = slot.saturating_sub(stake.points).saturating_add(new_pts);
            stake.points = new_pts;
            stake.earnings_snap = net_delta;
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
            if *backing_ledger.owner != config.percolator_program {
                return Err(ProgramError::IllegalOwner);
            }
            let data = backing_ledger.try_borrow_data()?;
            validate_portfolio_identity(&config, &data, &stake.owner)?;
            let (long_paid, _, short_paid, _) = read_portfolio_funding_flow(&data)?;
            let counter = funding_payer_counter(long_paid, short_paid);
            let net_delta = counter.saturating_sub(stake.residual_snap);
            let new_pts = net_delta;
            let slot = config.cohort_points_mut(stake.cohort);
            *slot = slot.saturating_sub(stake.points).saturating_add(new_pts);
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
// must be a token account OWNED BY this rd_config PDA, holding the full fixed supply (EZ), with the
// coin_mint carrying NO mint or freeze authority (GX) so the supply can't be inflated or frozen under
// the claimers. double-freeze is rejected so neither the snapshot nor the vault can be moved.
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
    if now
        < config
            .emission_end_slot
            .saturating_add(config.finalize_window)
    {
        return Err(ProgramError::InvalidInstructionData); // emission + finalize window still open
    }
    // GX: the COIN is a fixed pool — no mint authority (can't inflate) and no freeze authority (can't
    // freeze a claimer's account). EZ: the bound vault is rd_config-owned and holds the WHOLE supply.
    if *coin_mint.key != config.coin_mint {
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
    if mint.mint_authority.is_some()
        || mint.freeze_authority.is_some()
        || mint.supply != config.total_supply
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let v = spl_token::state::Account::unpack(&vault.try_borrow_data()?)?;
    // owner == rd_config + funded with the whole supply (EZ). No delegate/close_authority check is
    // needed: SPL's set_authority(AccountOwner) clears delegate + delegated_amount + close_authority,
    // and rd_config (a PDA with no approve instruction) can never set them — so a vault handed to
    // rd_config is SOLELY rd-controlled. The freeze vault/mint guards (owner, full funding, no mint/freeze
    // authority) are pinned by the e2e test `freeze_enforces_fixed_supply_and_vault_integrity`.
    if v.owner != *config_account.key
        || v.mint != config.coin_mint
        || v.amount < config.total_supply
    {
        return Err(ProgramError::InvalidAccountData);
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
//   LP/trader cohorts append one more: the Percolator portfolio (for the residual live cap).
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
// close or dematerialize flat portfolios after crystallize/freeze. The share-value and residual cohorts
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
    if stake.claimed {
        return Err(ProgramError::InvalidAccountData); // double-claim
    }
    // Share-value cohorts (insurance/backing) cap the payout by LIVE shares at claim time, so the claim
    // SLOT is value-relevant. claim is otherwise permissionless; if a third party could trigger a
    // share-value claim they could force it during a transient low-share moment — e.g. mid partial
    // insurance-withdraw, which leaves the Position withdrawn=false but shares reduced — and the
    // irreversible claimed-flag would lock in the reduced (or zero) payout, stranding the remainder
    // (finding KM). So a share-value claim must be authorized by the stake's OWN owner (the depositor who
    // controls the shares and bears the soft-veto timing). Portfolio-flow cohorts pay frozen points from
    // account counters, so their claim slot is irrelevant and stays permissionless.
    if matches!(stake.cohort, COHORT_INSURANCE | COHORT_BACKING) && cranker.key != &stake.owner {
        return Err(ProgramError::MissingRequiredSignature);
    }
    // The COIN must land in the bound recipient's own account (finding GY: no cranker redirect).
    let ra = spl_token::state::Account::unpack(&recipient_ata.try_borrow_data()?)?;
    if ra.owner != stake.recipient || ra.mint != config.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    let cohort_supply = config.cohort_supply(stake.cohort);
    let frozen_denom = config.frozen_cohort_points(stake.cohort);
    let amount = match stake.cohort {
        COHORT_INSURANCE | COHORT_BACKING => {
            // Share-value cohort: read the LIVE Position shares NOW and cap by them ATOMICALLY (finding
            // HE/JC + soft veto). A depositor who redeemed shares after freeze -> fewer live shares ->
            // claims less; a full exit -> 0 shares -> 0 COIN (forfeit). The appended account is the
            // bound subledger position. read+cap+pay in ONE tx, so there is no finalize/claim over-claim gap.
            let position = next_account_info(iter)?;
            if *position.key != stake.backing_ledger || *position.owner != config.subledger_program
            {
                return Err(ProgramError::InvalidAccountData);
            }
            let (shares, withdrawn) = read_subledger_shares(&position.try_borrow_data()?)?;
            let live_pts = share_value_points(shares, withdrawn);
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
            if *portfolio.key != stake.backing_ledger
                || *portfolio.owner != config.percolator_program
            {
                return Err(ProgramError::InvalidAccountData);
            }
            let data = portfolio.try_borrow_data()?;
            validate_portfolio_identity(&config, &data, &stake.owner)?;
            let (received, crystallized, spent) = read_portfolio_residual(&data)?;
            let live_net = residual_counter(stake.cohort, received, crystallized, spent)
                .saturating_sub(stake.residual_snap);
            let frozen_net = stake.earnings_snap; // net_delta captured at the last crystallize
            let cap_net = if stake.cohort == COHORT_TRADER {
                let spent_since_crystallize = spent.saturating_sub(stake.eligible_accum);
                core::cmp::min(frozen_net.saturating_sub(spent_since_crystallize), live_net)
            } else {
                live_net
            };
            let pts = if frozen_net == 0 || cap_net >= frozen_net {
                stake.points
            } else {
                stake.points.saturating_mul(cap_net) / frozen_net
            };
            points_to_amount(cohort_supply, pts, frozen_denom)
        }
        _ => {
            // Funding-payer cohort: crystallize authenticated the bound Percolator portfolio and froze points
            // from monotonic paid-funding counters. Do not require the portfolio again at claim time; a flat
            // portfolio may be closed or terminal-cleaned before the owner claims COIN, and the frozen numerator
            // plus frozen denominator already conserve the cohort.
            points_to_amount(cohort_supply, stake.points, frozen_denom)
        }
    };
    // ANTI-WASH FEE (finding NZ): portfolio-flow cohorts are farmable by synthetic/wash flow on allow-listed
    // markets, so they pay a fee retained in the vault. Share-value cohorts are capital-at-risk and pay no fee.
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
        let signer_seeds: [&[u8]; 3] = [b"rd_config", config.coin_mint.as_ref(), &bump_arr];
        invoke_signed(
            &spl_token::instruction::transfer(
                token_program.key,
                vault.key,
                recipient_ata.key,
                config_account.key,
                &[],
                payout,
            )?,
            &[
                vault.clone(),
                recipient_ata.clone(),
                config_account.clone(),
                token_program.clone(),
            ],
            &[&signer_seeds],
        )?;
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
    fn reads_live_subledger_shares_offsets() {
        let mut d = [0u8; 120];
        d[88] = 1; // withdrawn
        d[104..120].copy_from_slice(&777u128.to_le_bytes()); // shares
        let (shares, w) = read_subledger_shares(&d).unwrap();
        assert_eq!(shares, 777);
        assert!(w);
        assert_eq!(share_value_points(777, true), 0, "withdrawn -> forfeit");
        assert_eq!(share_value_points(777, false), 777, "live -> shares");
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
