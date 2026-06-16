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
//! `net_delta = counter - residual_snap`, weighted by `floor(log2(now - start_slot))`.
//! The `counter` is the NET-BY-SPENT drain `crystallized - spent` for the TRADER cohort
//! (finding NZ: a delta-neutral wash recovers its own crystallized loss only by churning,
//! which spends its OWN budget → spent rises → net ≈ 0) and `received` for the LP cohort.
//! ANTI-WASH = (1) the net-by-spent counter above + (2) the claim-fee `fee_support_bps`
//! retained in the vault on every LP/trader claim (the LP `received` has no symmetric net,
//! so the claim-fee taxes it) + (3) the claim LIVE-CAP: claim re-reads the bound portfolio
//! and scales the frozen points by `min(1, live_net/frozen_net)`, so a loss crystallized at
//! peak then recovered (net drops) pays proportionally less — closing the stale-points
//! bypass of net-by-spent. The tenure weight (registration age) is computed here, so JIT
//! capture is damped with no percolator start-slot field. NOTE: an earlier design used a
//! per-window `eligible = min(Δresidual, Δfee*10000/bps)` fee-cap (the `earnings_snap` /
//! `eligible_accum` fields); it was SUPERSEDED by net-by-spent + the claim-fee. `earnings_snap`
//! stores the crystallize-time `net_delta`, and `eligible_accum` stores trader `spent` at
//! crystallize so fresh post-freeze exposure cannot revive recovered frozen losses.
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
#[allow(unused_imports)]
use alloc::format; // required by entrypoint!/msg! in SBF builds

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
// Up to this many ADDITIONAL allow-listed markets beyond the primary `market_group` (finding IL+): the LP
// and trader cohorts read percolator portfolio counters that an attacker can manufacture if they control the
// market's oracle, so a portfolio is only countable if its market is on this orchestrator-vetted allow-list.
// The creator stands up N trusted-Pyth markets while holding the market-auth key locally, vets them, then
// transfers that key to the PDA that rotates it to the DAO — so the allow-listed markets cannot later be
// repointed at an attacker oracle. See DESIGN.md "Market allow-list".
const MAX_EXTRA_MARKETS: usize = 9; // 10 total allow-listed markets (market_group + 9 extras)
const CONFIG_SIZE: usize = 466 + 1 + MAX_EXTRA_MARKETS * 32; // base 466 + extra_market_count(1) + extras(9*32) = 755
const STAKE_SIZE: usize = 211; // +1 claimed flag (self-service)
// 4-cohort deterministic model (10/10/40/40). Insurance & backing reward SHARE VALUE (subledger
// Position.shares — pro-rata with fees, soft-veto on exit); LP & trader reward the percolator
// PortfolioAccountV16 residual counters (received / crystallized_loss — monotonic, real-loss-backed,
// un-gameable). See tests/offsets.rs for the pinned reads.
const COHORT_INSURANCE: u8 = 0;
const COHORT_BACKING: u8 = 1;
const COHORT_LP: u8 = 2;
const COHORT_TRADER: u8 = 3;

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
/// (`points_i = floor_log2(tenure) * net_delta`), so we `saturating_mul` rather than use an unchecked `*`
/// (which would PANIC = brick every claim in the cohort) or a wrapping `*` (which would DRAIN). Because
/// `points_i <= total_points` for any real stake, the result is ALWAYS `<= total_supply`, so this never
/// overpays/over-allocates the fixed pool regardless of saturation. Saturation is itself unreachable for
/// realistic inputs (`net_delta` is the crystallized residual, bounded by the market's collateral, far below
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
    let b = data.get(off..off + 16).ok_or(ProgramError::AccountDataTooSmall)?;
    Ok(u128::from_le_bytes(b.try_into().unwrap()))
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
    let withdrawn = *data.get(SUB_POS_WITHDRAWN).ok_or(ProgramError::AccountDataTooSmall)? == 1;
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
// percolator PortfolioAccountV16Account snapshot read (LP & trader cohorts) — offsets PINNED
// ===========================================================================
// Account = HEADER_LEN(16) + repr(C) PortfolioAccountV16Account { provenance_header(100), owner[32]@100,
// capital@132, pnl@148, reserved_pnl@164, residual_crystallized_loss_atoms_total@180,
// residual_spent_principal_atoms_total@196, residual_received_atoms_total@212, ... }. Absolute = 16 +
// within-struct. PINNED against the real struct by tests/offsets.rs (offset_of! + HEADER_LEN).
// LP cohort reads `received` (residual the matcher absorbed); trader cohort reads the NET drain
// `crystallized_loss - spent` (loss that actually hit the backstop, NOT loss a counterparty later recovered).
// NETTING (finding NZ): crystallized_loss alone is wash-farmable — a delta-neutral self-deal (long+short, both
// miner-owned) crystallizes a REAL loss on the long that is REAL gain on the short, so the miner's NET capital
// is zero yet the gross loss counter rises. The long's crystallized loss is recovered by the short's matched
// fill (long.spent rises to == crystallized), so `crystallized - spent == 0` for the washed leg: a self-deal
// earns NO trader points. Only loss that drained insurance with no counterparty recovery (spent < crystallized)
// counts. The LP `received` leg has no symmetric per-account recovery counter, so it is netting-resistant and
// is bounded instead by the claim fee (see process_claim). spent <= crystallized is shape-validated by percolator.
// CROSS-ACCOUNT CONSERVATION (percolator v16.rs transfer_account_residual_reward_credit, via
// transfer_trade_residual_reward_credit on a position INCREASE): when a trader with an outstanding net loss
// (crystallized - spent) re-trades, `credit = min(net, counterparty_increased_margin)` is moved — the trader's
// `spent` rises by credit AND the counterparty's `received` rises by the SAME credit. So `received` is NOT free:
// it is exactly the trader crystallized loss transferred to an LP that committed real margin to back it, it is
// globally bounded by the trader's REAL crystallized loss (each transfer raises spent, depleting `net`, so the
// same loss can never be transferred twice), and it is ZERO-SUM with the trader cohort's net-by-spent. An
// attacker self-dealing trader+LP cannot double-count the SAME loss across both cohorts: the trader leg's frozen
// points are capped to its live net at claim (the live-cap, defense (3)) — pinned by
// cross_cohort_trader_loss_then_lp_recovery_cannot_double_dip_the_same_loss. So the LP cohort, like the trader
// cohort, costs real capital-at-risk + the claim fee; it is the LEAST farmable, not a free counter.
// The portfolio's provenance market_group_id is the FIRST field of the struct, so it sits right after
// the percolator account header. The LP/trader cohorts MUST scope to it (finding IL): the residual
// counters are admin-mark-manipulable on a market whose oracle the registrant controls, so a portfolio
// is only countable if it belongs to the ONE allow-listed (trusted-Pyth) genesis market the orchestrator
// bound at init (config.market_group). Without this an attacker stands up their OWN percolator market with
// an auth-mark oracle they push, self-trades to mint crystallized_loss/received, and farms the COIN.
pub const OFF_PORTFOLIO_MARKET_GROUP: usize = PERC_HEADER_LEN;
pub const OFF_PORTFOLIO_OWNER: usize = PERC_HEADER_LEN + 100;
pub const OFF_PORTFOLIO_CRYSTALLIZED_LOSS: usize = PERC_HEADER_LEN + 180;
pub const OFF_PORTFOLIO_SPENT: usize = PERC_HEADER_LEN + 196;
pub const OFF_PORTFOLIO_RECEIVED: usize = PERC_HEADER_LEN + 212;

/// (residual_received, residual_crystallized_loss, residual_spent_principal) from a live percolator
/// PortfolioAccount. `spent` is how much of this account's crystallized loss a counterparty later recovered
/// (spent <= crystallized, shape-validated) — i.e. the WASHED/transferred portion, subtracted from the
/// trader cohort so a delta-neutral self-deal nets to zero (finding NZ).
pub fn read_portfolio_residual(data: &[u8]) -> Result<(u128, u128, u128), ProgramError> {
    Ok((
        read_u128(data, OFF_PORTFOLIO_RECEIVED)?,
        read_u128(data, OFF_PORTFOLIO_CRYSTALLIZED_LOSS)?,
        read_u128(data, OFF_PORTFOLIO_SPENT)?,
    ))
}

/// The cohort's residual COUNTER: trader = NET drain (crystallized - spent, so a washed/recovered loss earns
/// nothing); LP = gross received (no per-account net counter exists; bounded by the claim fee instead).
fn residual_counter(cohort: u8, received: u128, crystallized: u128, spent: u128) -> u128 {
    if cohort == COHORT_LP {
        received
    } else {
        crystallized.saturating_sub(spent)
    }
}

/// floor(log2(n)); 0 for n < 2. The residual time-weight multiplier (parity with genesis-vote's
/// floor(log2(hold_time)) and the rd's original GZ design).
fn floor_log2(n: u64) -> u128 {
    if n < 2 { 0 } else { (63 - n.leading_zeros()) as u128 }
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
    total_points: u128,        // residual-backing cohort
    sealed: u8,
    bump: u8,
    insurance_bps: u16,        // insurance cohort's share of supply (e.g. 2000 = 20%)
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
    // ---- 4-cohort tail (10/10/40/40). `total_points`/`insurance_total_points` above are the BACKING
    // and INSURANCE cohort point totals; these add the LP and TRADER cohorts + their bps + the backing
    // pool scope. trader_bps is implicit (10000 - insurance - backing - lp). ----
    backing_pool: Pubkey,           // the genesis BACKING subledger pool (DOMAIN_BACKING) the backing cohort is scoped to
    backing_bps: u16,               // backing cohort supply share
    lp_bps: u16,                    // LP cohort supply share
    lp_total_points: u128,          // LP cohort (PortfolioAccount residual_received Δ)
    trader_total_points: u128,      // trader cohort (PortfolioAccount residual_crystallized_loss Δ)
    frozen_lp_total_points: u128,
    frozen_trader_total_points: u128,
    // ---- market allow-list tail (finding IL+) ----
    // The LP/trader cohorts count a portfolio ONLY if its provenance market_group is allow-listed. The
    // primary entry is `market_group` above; these are 0..=MAX_EXTRA_MARKETS additional trusted markets the
    // orchestrator vetted at init. `extra_market_count` of them are live (the rest are Pubkey::default()).
    extra_market_count: u8,
    extra_markets: [Pubkey; MAX_EXTRA_MARKETS],
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
                let mut a = [Pubkey::default(); MAX_EXTRA_MARKETS];
                let mut i = 0;
                while i < MAX_EXTRA_MARKETS {
                    a[i] = pk(d, 467 + i * 32);
                    i += 1;
                }
                a
            },
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
        for (i, m) in self.extra_markets.iter().enumerate() {
            d[467 + i * 32..467 + i * 32 + 32].copy_from_slice(m.as_ref());
        }
    }
    /// Is `m` an allow-listed (orchestrator-vetted trusted-Pyth) market for the LP/trader cohorts? The
    /// primary `market_group` plus the first `extra_market_count` extras. Default is never allowed.
    fn market_allowed(&self, m: &Pubkey) -> bool {
        if *m == Pubkey::default() {
            return false;
        }
        if *m == self.market_group {
            return true;
        }
        self.extra_markets[..self.extra_market_count as usize].contains(m)
    }
    /// trader bps = the remainder (so the four cohorts always sum to exactly 100%).
    fn trader_bps(&self) -> u16 {
        (BPS_DENOMINATOR as u16)
            .saturating_sub(self.insurance_bps)
            .saturating_sub(self.backing_bps)
            .saturating_sub(self.lp_bps)
    }
    /// COIN supply allocated to a cohort.
    fn cohort_supply(&self, cohort: u8) -> u64 {
        let bps = match cohort {
            COHORT_INSURANCE => self.insurance_bps,
            COHORT_BACKING => self.backing_bps,
            COHORT_LP => self.lp_bps,
            _ => self.trader_bps(),
        } as u128;
        ((self.total_supply as u128) * bps / BPS_DENOMINATOR as u128) as u64
    }
    /// Live running point total for a cohort (mutated in register/crystallize).
    fn cohort_points_mut(&mut self, cohort: u8) -> &mut u128 {
        match cohort {
            COHORT_INSURANCE => &mut self.insurance_total_points,
            COHORT_BACKING => &mut self.total_points,
            COHORT_LP => &mut self.lp_total_points,
            _ => &mut self.trader_total_points,
        }
    }
    /// Frozen denominator for a cohort (snapshotted at freeze; used by claim).
    fn frozen_cohort_points(&self, cohort: u8) -> u128 {
        match cohort {
            COHORT_INSURANCE => self.frozen_insurance_total_points,
            COHORT_BACKING => self.frozen_total_points,
            COHORT_LP => self.frozen_lp_total_points,
            _ => self.frozen_trader_total_points,
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
    cohort: u8, // COHORT_RESIDUAL | COHORT_INSURANCE. For insurance, `backing_ledger` is the
                // subledger position and `recipient` is the depositor.
    // TRADER live-cap anchor: crystallize stores the absolute `spent` counter here so claim can distinguish
    // still-unrecovered frozen loss from fresh post-freeze net. Held at 0 for non-trader cohorts. Repurposed
    // from the superseded fee-cap design without changing the serialized layout.
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

// ===========================================================================
#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (tag, rest) = data.split_first().ok_or(ProgramError::InvalidInstructionData)?;
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

    // 4-cohort wire (10/10/40/40 default): total_supply, emission_end, insurance_bps, backing_bps,
    // lp_bps (trader = remainder), finalize_window, subledger_pool (insurance), backing_pool, market_group.
    let total_supply = take_u64(&mut data)?;
    let emission_end_slot = take_u64(&mut data)?;
    let insurance_bps = take_u16(&mut data)?;
    let backing_bps = take_u16(&mut data)?;
    let lp_bps = take_u16(&mut data)?;
    let finalize_window = take_u64(&mut data)?;
    let subledger_pool = take_pubkey(&mut data)?; // insurance pool (DOMAIN_INSURANCE), finding HG scope
    let backing_pool = take_pubkey(&mut data)?; // backing pool (DOMAIN_BACKING) scope
    let market_group = take_pubkey(&mut data)?; // primary allow-listed market (LP/trader scope, finding IL)
    // Market allow-list tail (finding IL+): a u8 count followed by that many ADDITIONAL trusted-Pyth market
    // pubkeys the orchestrator vetted. Bounded by MAX_EXTRA_MARKETS; each must be a real, distinct key.
    let extra_market_count = *data.first().ok_or(ProgramError::InvalidInstructionData)?;
    data = &data[1..];
    if extra_market_count as usize > MAX_EXTRA_MARKETS {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut extra_markets = [Pubkey::default(); MAX_EXTRA_MARKETS];
    for slot in extra_markets.iter_mut().take(extra_market_count as usize) {
        let m = take_pubkey(&mut data)?;
        if m == Pubkey::default() || m == market_group {
            return Err(ProgramError::InvalidInstructionData); // no default / duplicate of the primary
        }
        *slot = m;
    }
    // OPTIONAL trailing residual_fee_bps (u16, finding NZ): the anti-wash fee skimmed from LP/trader claims
    // (process_claim). Absent (no trailing bytes) = DEFAULT_FEE_SUPPORT_BPS; explicit 0 remains allowed.
    let residual_fee_bps = if data.len() == 2 { take_u16(&mut data)? } else { DEFAULT_FEE_SUPPORT_BPS };
    if residual_fee_bps > BPS_DENOMINATOR as u16 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !data.is_empty() || !payer.is_signer || total_supply == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    // The four cohort shares must not exceed 100% (trader takes the remainder, so sum <= 10000).
    if (insurance_bps as u32) + (backing_bps as u32) + (lp_bps as u32) > BPS_DENOMINATOR as u32 {
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
    // The LP and trader cohorts read percolator portfolio counters that are admin-mark-manipulable on a
    // market whose oracle the registrant controls, so they MUST be scoped to the one allow-listed
    // trusted-Pyth genesis market (finding IL). If either cohort has a share (lp_bps > 0, or trader =
    // the remainder > 0), market_group must be a real key — never default (which register treats as
    // "any market" and would let an attacker's self-oracle'd market farm the COIN).
    let trader_bps = (BPS_DENOMINATOR as u32)
        .saturating_sub(insurance_bps as u32 + backing_bps as u32 + lp_bps as u32);
    if (lp_bps > 0 || trader_bps > 0) && market_group == Pubkey::default() {
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
    if mint.mint_authority != COption::Some(*coin_mint_authority.key) || !coin_mint_authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let bump_arr = [bump];
    let seeds: [&[u8]; 3] = [b"rd_config", coin_mint.key.as_ref(), &bump_arr];
    create_pda(payer, config_account, system, program_id, &seeds, CONFIG_SIZE)?;
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
    }
    .serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// register_start accounts: [payer(s,w), config, owner, recipient, linked, stake(pda,w), system]
//   residual:  linked = percolator backing ledger; insurance: linked = subledger position.
// data: cohort(u8)
fn register_start(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let cohort = *data.first().ok_or(ProgramError::InvalidInstructionData)?;
    if cohort > COHORT_TRADER {
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
    // `snap` is the register-time counter snapshot: 0 for the share-value cohorts (insurance/backing —
    // points are the LIVE shares read at crystallize/claim), and the portfolio residual counter for the
    // LP/trader cohorts (Δ measured at crystallize).
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
            // Residual cohort (LP/trader): `linked` is a percolator PortfolioAccount. (Scope is
            // account-level — the residual counters are per-account totals; the genesis is single-market,
            // so account-level == this-market. market_group is informational here.)
            if *linked.owner != config.percolator_program {
                return Err(ProgramError::IllegalOwner); // counters must be percolator-authenticated
            }
            let data = linked.try_borrow_data()?;
            // Bind the portfolio to its owner (finding GY).
            if pk(&data, OFF_PORTFOLIO_OWNER) != *owner.key {
                return Err(ProgramError::IllegalOwner);
            }
            // Scope to the allow-listed (trusted-Pyth) markets (finding IL+). The residual counters are
            // wash-manufacturable on a market whose oracle the registrant controls, so a portfolio only
            // counts if its provenance market is on config's orchestrator-vetted allow-list (market_group +
            // extras). An attacker's own auth-mark market is rejected here.
            if !config.market_allowed(&pk(&data, OFF_PORTFOLIO_MARKET_GROUP)) {
                return Err(ProgramError::IllegalOwner);
            }
            let (received, crystallized, spent) = read_portfolio_residual(&data)?;
            residual_counter(cohort, received, crystallized, spent)
        }
    };
    let start_slot = now;
    let (expected, bump) = Pubkey::find_program_address(
        &[b"rd_stake", config_account.key.as_ref(), owner.key.as_ref()],
        program_id,
    );
    if *stake_account.key != expected || stake_account.data_len() != 0 {
        return Err(ProgramError::InvalidSeeds);
    }
    let bump_arr = [bump];
    let seeds: [&[u8]; 4] = [b"rd_stake", config_account.key.as_ref(), owner.key.as_ref(), &bump_arr];
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

    if !cranker.is_signer
        || config_account.owner != program_id
        || stake_account.owner != program_id
    {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config.sealed != 0 || config.freeze_slot != 0 {
        return Err(ProgramError::InvalidAccountData); // sealed or frozen -> denominators are final
    }
    let mut stake = Stake::deserialize(&stake_account.try_borrow_data()?)?;
    if stake.config != *config_account.key || stake.backing_ledger != *backing_ledger.key {
        return Err(ProgramError::InvalidAccountData);
    }
    // subtract-old/add-new keeps the cohort denominator authoritative as points are re-derived. The
    // claim-time live cap (insurance/backing: live shares; not needed for LP/trader monotonic counters)
    // is the backstop against any stale-high denominator contribution.
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
            // (LP/trader stay permissionless — their counters are monotonic, so a forced crystallize can
            // only raise the Δ, never grief.)
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
        _ => {
            // Residual cohort (LP/trader): points = TIME-WEIGHTED Δ(net counter) since register. The TRADER
            // counter is the NET drain `crystallized - spent` (finding NZ): a delta-neutral self-deal has its
            // crystallized loss recovered by its OWN counterparty leg only if it churns (close/reopen spends its
            // own budget -> spent rises -> net 0). The LP counter (`received`) has no symmetric net and is also
            // bounded by the claim fee. TIME-WEIGHT (GZ): points = floor(log2(now - start_slot)) * netΔ, so the
            // budget must stay OUTSTANDING (position open, capital locked) as long as possible to earn — and
            // churning to recycle capital spends the budget (spent up -> net down) for nothing. Parity with the
            // genesis-vote floor(log2(hold)) * principal weight.
            if *backing_ledger.owner != config.percolator_program {
                return Err(ProgramError::IllegalOwner);
            }
            let (received, crystallized, spent) = read_portfolio_residual(&backing_ledger.try_borrow_data()?)?;
            let counter = residual_counter(stake.cohort, received, crystallized, spent);
            let net_delta = counter.saturating_sub(stake.residual_snap);
            let tenure = Clock::get()?.slot.saturating_sub(stake.start_slot);
            let new_pts = floor_log2(tenure).saturating_mul(net_delta);
            let slot = config.cohort_points_mut(stake.cohort);
            *slot = slot.saturating_sub(stake.points).saturating_add(new_pts);
            stake.points = new_pts;
            // Record the net_delta behind these points so CLAIM can cap proportionally to a later net DROP
            // without re-deriving the frozen time-weight. For traders, also remember `spent` at crystallize:
            // later spent growth recovers the frozen loss and must not be replaceable with fresh live net.
            stake.earnings_snap = net_delta;
            stake.eligible_accum = if stake.cohort == COHORT_TRADER { spent } else { 0 };
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
    if now < config.emission_end_slot.saturating_add(config.finalize_window) {
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
    if mint.mint_authority.is_some() || mint.freeze_authority.is_some() || mint.supply != config.total_supply {
        return Err(ProgramError::InvalidAccountData);
    }
    let v = spl_token::state::Account::unpack(&vault.try_borrow_data()?)?;
    // owner == rd_config + funded with the whole supply (EZ). No delegate/close_authority check is
    // needed: SPL's set_authority(AccountOwner) clears delegate + delegated_amount + close_authority,
    // and rd_config (a PDA with no approve instruction) can never set them — so a vault handed to
    // rd_config is SOLELY rd-controlled. The freeze vault/mint guards (owner, full funding, no mint/freeze
    // authority) are pinned by the e2e test `freeze_enforces_fixed_supply_and_vault_integrity`.
    if v.owner != *config_account.key || v.mint != config.coin_mint || v.amount < config.total_supply {
        return Err(ProgramError::InvalidAccountData);
    }
    config.vault = *vault.key;
    // Snapshot all four cohort denominators.
    config.frozen_insurance_total_points = config.insurance_total_points;
    config.frozen_total_points = config.total_points; // BACKING
    config.frozen_lp_total_points = config.lp_total_points;
    config.frozen_trader_total_points = config.trader_total_points;
    config.freeze_slot = now;
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// claim accounts: [cranker(s), config, stake(w), vault(w), recipient_ata(w), token_program]
//   insurance cohort appends one more: the subledger position (for the live HE cap).
//
// PERMISSIONLESS self-service residual claim (replaces the cranker-assembled seal for the residual
// cohort). Pays the stake's OWN deterministic share —
// `residual_supply * stake.points / frozen_total_points` — to the stake's BOUND recipient, then marks
// it claimed. Each backer pulls their own slice; nobody assembles a global list, so there is no
// one-tx completeness seal (IG dissolved) and no cranker can omit or redirect a backer (the recipient
// is bound at register, finding GY, and re-checked here). Sum of all residual claims <= residual_supply
// (floor math), so the vault can never be over-drawn. The residual cohort uses the crystallized,
// now-frozen `stake.points` (cumulative loss — only ever grows), so there is no live-position
// dependency and no HE concern; the insurance cohort (live, HE-capped) is handled separately.
fn claim(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let stake_account = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let recipient_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if !cranker.is_signer || config_account.owner != program_id || stake_account.owner != program_id {
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
    if stake.config != *config_account.key {
        return Err(ProgramError::InvalidAccountData);
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
    // controls the shares and bears the soft-veto timing). LP/trader pay frozen points with no live cap,
    // so their claim slot is irrelevant and stays permissionless (any cranker may finalize them).
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
            if *position.key != stake.backing_ledger || *position.owner != config.subledger_program {
                return Err(ProgramError::InvalidAccountData);
            }
            let (shares, withdrawn) = read_subledger_shares(&position.try_borrow_data()?)?;
            let live_pts = share_value_points(shares, withdrawn);
            let pts = if stake.points < live_pts { stake.points } else { live_pts };
            points_to_amount(cohort_supply, pts, frozen_denom)
        }
        _ => {
            // Residual cohort (LP/trader): LIVE-CAP the frozen points against a post-crystallize NET DROP.
            // The TRADER counter (crystallized - spent) is NOT monotonic — a loss-recovery (spent rises) drops
            // the net. Without re-reading, a farmer could crystallize at PEAK net, recover the loss to net 0,
            // SKIP the re-crystallize, and claim the stale-high FROZEN points — bypassing net-by-spent and
            // diluting honest co-claimants (finding: stale-points wash). Re-read the bound portfolio and scale
            // the points by the still-valid frozen net: recovery after crystallize lowers the payout
            // proportionally, while the frozen TIME-WEIGHT (already baked into stake.points) is preserved.
            // For traders, the crystallize-time `spent` anchor prevents fresh post-freeze net from reviving
            // points whose original frozen loss was already recovered. LP's `received` is monotonic, so the
            // cap remains a harmless no-op there.
            let portfolio = next_account_info(iter)?;
            if *portfolio.key != stake.backing_ledger || *portfolio.owner != config.percolator_program {
                return Err(ProgramError::InvalidAccountData);
            }
            let (received, crystallized, spent) = read_portfolio_residual(&portfolio.try_borrow_data()?)?;
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
    };
    // ANTI-WASH FEE (finding NZ): the LP/trader (PnL-flow) cohorts are wash-farmable via a delta-neutral
    // self-deal — the long crystallizes a REAL loss (drains the backstop) while the short takes the offsetting
    // gain, so the miner's NET capital is zero yet the loss counter rises and earns COIN. NO on-chain counter
    // nets the two legs (the trader spent-netting only catches matched-fill self-recovery; the offsetting short
    // gain lives in `pnl`, not in any residual counter). So these cohorts are taxed: a fee_support_bps fraction
    // of each LP/trader payout is RETAINED in the vault (removed from the claimant, locked = deflationary),
    // making manufactured residual always cost a fraction of what it earns. The share-value cohorts
    // (insurance/backing) are capital-at-risk, not PnL-flow, so they pay NO fee.
    let fee = if matches!(stake.cohort, COHORT_LP | COHORT_TRADER) {
        // CEIL the fee, not floor: a flooring fee rounds to 0 on dust claims (amount*bps < BPS_DENOMINATOR),
        // letting a Sybil farmer FRAGMENT one farm into many dust stakes that EACH pay 0 fee and dodge the
        // anti-wash skim entirely (the fee is the sole economic bound on the delta-neutral cross-margin wash).
        // Ceiling makes every nonzero LP/trader claim pay >= 1 atom, so fragmentation can never reduce the
        // effective fee below the intended rate. fee <= amount holds (bps <= BPS_DENOMINATOR):
        // ceil(amount*bps/DEN) <= amount, so the payout subtraction never underflows.
        ((amount as u128) * (config.fee_support_bps as u128)).div_ceil(BPS_DENOMINATOR as u128) as u64
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
        assert!(points_to_amount(1_000_000, 30, 100) + points_to_amount(1_000_000, 70, 100) <= 1_000_000);
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
}
