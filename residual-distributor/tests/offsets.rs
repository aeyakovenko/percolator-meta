//! LOAD-BEARING canary suite (finding T) — KEEP IN THE SUITE. The decider's hardcoded snapshot offsets
//! MUST equal HEADER_LEN + offset_of!(real percolator struct, field), and its subledger Position offsets
//! MUST equal the subledger's canonical layout. If percolator reorders PortfolioAccountV16Account or the
//! subledger reorders Position, these fail — preventing the GT/HF-class drift where a consumer reads at
//! stale offsets against a rebuilt dependency. A drift that silently shifted the residual reward reads is a
//! free-farm/DoS (e.g. `spent` reading an always-0 field stops penalizing churn; reading a large field nets
//! every trader claim to 0), so this canary is the SOLE structural guard against it — do not delete.
//!
//! (The earlier "[branch-only, DO NOT PUSH]" header was STALE: this file is committed on master, passes, and
//! is actively maintained — the whole workspace already builds from local-path git deps, so it carries no
//! extra portability coupling. It must ship with the suite.)

use core::mem::offset_of;
use percolator::PortfolioAccountV16Account as P;
use residual_distributor::{
    points_to_amount, OFF_PORTFOLIO_CRYSTALLIZED_LOSS, OFF_PORTFOLIO_FUNDING_LONG_PAID,
    OFF_PORTFOLIO_FUNDING_LONG_RECEIVED, OFF_PORTFOLIO_FUNDING_SHORT_PAID,
    OFF_PORTFOLIO_FUNDING_SHORT_RECEIVED, OFF_PORTFOLIO_MARKET_GROUP, OFF_PORTFOLIO_OWNER,
    OFF_PORTFOLIO_RECEIVED, OFF_PORTFOLIO_SPENT, PERC_HEADER_LEN,
};

// OVERFLOW SAFETY of the pro-rata split: total_supply (u64) * points_i (u128) can exceed u128 when points_i is
// large; points_to_amount must SATURATE (never panic = brick the cohort's claims, never wrap = drain) and the
// result must always be bounded by total_supply (points_i <= total_points => share <= total_supply). Pins that the
// saturating_mul is not "simplified" to an unchecked `*` (a brick/drain regression an ordinary-magnitude test misses).
#[test]
fn points_to_amount_is_overflow_safe_never_panics_and_never_over_allocates() {
    // Extreme inputs that overflow a naive u64*u128 product: must not panic, and (points_i <= total_points so)
    // the share is always bounded by total_supply — never an over-allocation/drain.
    assert!(
        points_to_amount(u64::MAX, u128::MAX, u128::MAX) <= u64::MAX,
        "max/max/max: no panic, bounded by supply"
    );
    let supply = 400_000u64;
    // Sole staker (points_i == total_points) at a magnitude that saturates total_supply*points_i: still <= supply.
    let huge = 7e31 as u128; // > u128::MAX / supply -> the product saturates
    assert!(
        points_to_amount(supply, huge, huge) <= supply,
        "even at saturation a sole staker is paid <= the cohort supply (no drain)"
    );
    // Sole staker below saturation: gets exactly the whole cohort.
    assert_eq!(
        points_to_amount(supply, 1_000_000, 1_000_000),
        supply,
        "non-saturating sole staker gets the whole cohort"
    );
    // total_points == 0 -> 0 (no div-by-zero), and an ordinary split is exact.
    assert_eq!(
        points_to_amount(supply, 5, 0),
        0,
        "zero denominator yields zero, no panic"
    );
    assert_eq!(
        points_to_amount(1_000_000, 117_000, 198_000),
        1_000_000u64 * 117_000 / 198_000,
        "ordinary split is exact"
    );
}

// LP & trader residual counters live in PortfolioAccountV16Account (read at HEADER_LEN..). PINNED so a
// percolator reorder of the portfolio header can't silently shift the residual reward reads.
#[test]
fn portfolio_residual_counter_offsets_match_the_real_percolator_struct() {
    assert_eq!(PERC_HEADER_LEN, 16, "percolator HEADER_LEN");
    assert_eq!(
        OFF_PORTFOLIO_MARKET_GROUP,
        PERC_HEADER_LEN
            + offset_of!(P, provenance_header)
            + offset_of!(percolator::ProvenanceHeaderV16Account, market_group_id),
        "portfolio provenance market_group (LP/trader Pyth-market scope) offset"
    );
    assert_eq!(
        OFF_PORTFOLIO_OWNER,
        PERC_HEADER_LEN + offset_of!(P, owner),
        "portfolio owner (LP/trader reward owner) offset"
    );
    assert_eq!(
        OFF_PORTFOLIO_CRYSTALLIZED_LOSS,
        PERC_HEADER_LEN + offset_of!(P, residual_crystallized_loss_atoms_total),
        "trader cohort: crystallized-loss counter offset"
    );
    assert_eq!(
        OFF_PORTFOLIO_RECEIVED,
        PERC_HEADER_LEN + offset_of!(P, residual_received_atoms_total),
        "LP cohort: residual-received counter offset"
    );
    assert_eq!(
        OFF_PORTFOLIO_SPENT,
        PERC_HEADER_LEN + offset_of!(P, residual_spent_principal_atoms_total),
        "trader cohort: residual-SPENT counter offset"
    );
}

// Funding-payer cohort counters live in PortfolioAccountV16Account too. PINNED so the paid side cannot drift
// into the received side.
#[test]
fn portfolio_funding_payer_counter_offsets_match_the_real_percolator_struct() {
    assert_eq!(
        OFF_PORTFOLIO_FUNDING_LONG_PAID,
        PERC_HEADER_LEN + offset_of!(P, funding_long_paid_atoms_total),
        "funding-payer cohort: funding-long-paid counter offset"
    );
    assert_eq!(
        OFF_PORTFOLIO_FUNDING_LONG_RECEIVED,
        PERC_HEADER_LEN + offset_of!(P, funding_long_received_atoms_total),
        "non-rewarded counter pinned so received side is not confused with paid side"
    );
    assert_eq!(
        OFF_PORTFOLIO_FUNDING_SHORT_PAID,
        PERC_HEADER_LEN + offset_of!(P, funding_short_paid_atoms_total),
        "funding-payer cohort: funding-short-paid counter offset"
    );
    assert_eq!(
        OFF_PORTFOLIO_FUNDING_SHORT_RECEIVED,
        PERC_HEADER_LEN + offset_of!(P, funding_short_received_atoms_total),
        "non-rewarded counter pinned so received side is not confused with paid side"
    );
}

// The pinned distribution program id (finding HK) must equal the real distribution program — a fake
// program would let a front-runner squat a canonical-looking distribution_config and brick the seal.
#[test]
fn pinned_distribution_program_id_matches_the_real_program() {
    assert_eq!(
        residual_distributor::DISTRIBUTION_PROGRAM_ID,
        distribution_program::id(),
        "pinned distribution program id must match the deployed distribution program"
    );
}

// The subledger Position offsets residual-distributor reads MUST match the subledger's canonical
// layout (finding HF: a wrong owner offset slipped past mocked tests). Cross-pinned to the
// subledger's exported POS_* consts (themselves canaried against Position::serialize there). Only the
// LIVE share-value reads (pool/owner/withdrawn/shares) remain — the old principal*log-time model reads
// were removed (finding KO-followup dead-code cleanup).
#[test]
fn subledger_position_offsets_match_the_real_subledger_layout() {
    use residual_distributor as rd;
    assert_eq!(
        rd::SUB_POS_POOL,
        subledger_program::POS_POOL_OFF,
        "Position.pool offset"
    );
    assert_eq!(
        rd::SUB_POS_OWNER,
        subledger_program::POS_OWNER_OFF,
        "Position.owner offset"
    );
    assert_eq!(
        rd::SUB_POS_WITHDRAWN,
        subledger_program::POS_WITHDRAWN_OFF,
        "Position.withdrawn offset"
    );
    assert_eq!(
        rd::SUB_POS_SHARES,
        subledger_program::POS_SHARES_OFF,
        "Position.shares (share-value) offset"
    );
}
