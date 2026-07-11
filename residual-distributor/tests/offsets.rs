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

// OVERFLOW SAFETY + EXACTNESS of the pro-rata split: total_supply (u64) * points_i (u128) can exceed u128 when
// points_i is large. points_to_amount must never panic/wrap, and must preserve the exact floor quotient rather
// than saturating the intermediate and permanently locking a claimant's COIN.
#[test]
fn points_to_amount_is_overflow_safe_never_panics_and_never_over_allocates() {
    assert_eq!(
        points_to_amount(u64::MAX, u128::MAX, u128::MAX),
        u64::MAX,
        "max/max/max sole staker receives the exact full supply"
    );
    let supply = 400_000u64;
    let huge = 7e31 as u128;
    assert_eq!(
        points_to_amount(supply, huge, huge),
        supply,
        "an overflowing sole-staker product remains exact"
    );
    let max_principal = u64::MAX as u128;
    assert_eq!(
        points_to_amount(u64::MAX, 2 * max_principal, 4 * max_principal),
        u64::MAX / 2,
        "an overflowing nontrivial split uses the exact floor quotient"
    );
    let third = u128::MAX / 3;
    assert_eq!(
        points_to_amount(u64::MAX, third, third * 3),
        u64::MAX / 3,
        "an overflowing one-third split remains exact"
    );
    assert_eq!(
        points_to_amount(u64::MAX, u128::MAX - 1, u128::MAX),
        u64::MAX - 1,
        "an overflowing near-total split preserves the one-atom floor remainder"
    );
    assert_eq!(
        points_to_amount(10, 11, 10),
        0,
        "an invalid numerator above its denominator can never over-allocate"
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
// subledger's exported POS_* consts (themselves canaried against Position::serialize there).
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
        rd::SUB_POS_PRINCIPAL,
        subledger_program::POS_PRINCIPAL_OFF,
        "Position.principal (cross-pool base-unit points) offset"
    );
    assert_eq!(
        rd::SUB_POS_WITHDRAWN_AMOUNT,
        subledger_program::POS_WITHDRAWN_AMOUNT_OFF,
        "Position terminal-return principal snapshot offset"
    );
    assert_eq!(
        rd::SUB_POS_WITHDRAWN,
        subledger_program::POS_WITHDRAWN_OFF,
        "Position.withdrawn offset"
    );
    assert_eq!(
        rd::SUB_POS_START_SLOT,
        subledger_program::POS_START_SLOT_OFF,
        "Position.start_slot (top-up reset clock) offset"
    );
    assert_eq!(
        rd::SUB_POS_TERMINAL_RETURNED,
        subledger_program::POS_TERMINAL_RETURNED_OFF,
        "Position permissionless terminal-return marker offset"
    );
    assert_eq!(
        rd::SUB_POS_TERMINAL_RETURN_SLOT,
        subledger_program::POS_TERMINAL_RETURN_SLOT_OFF,
        "Position permissionless terminal-return slot offset"
    );
}
