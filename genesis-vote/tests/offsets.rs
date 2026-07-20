//! [branch-only, DO NOT PUSH] Finding ID — the gv counterpart of residual's offsets.rs (HL).
//! genesis-vote reads the subledger Position principal for vote WEIGHT and the subledger
//! Pool (outstanding_principal) for QUORUM, via a hardcoded byte-offset MIRROR (gv depends on neither
//! crate at runtime). If the subledger reorders those structs, the subledger's own canaries + this
//! cross-pin fail — preventing the HF-class drift where gv silently reads the wrong field and
//! miscomputes governance weight/quorum (capture/LOF). Also pins finding IC's hardcoded distribution
//! program id against the real deployed program.

use genesis_vote_program::{
    SUB_POOL_BOOTSTRAP_DELAY_OFF, SUB_POOL_DEPOSIT_DEADLINE_OFF,
    SUB_POOL_DEPOSIT_START_OFF, SUB_POOL_DEPOSIT_WINDOW_OFF, SUB_POOL_OUTSTANDING_OFF,
    SUB_POS_ACTION_NONCE_OFF, SUB_POS_OWNER_OFF, SUB_POS_POOL_OFF, SUB_POS_PRINCIPAL_OFF,
    SUB_POS_START_SLOT_OFF,
};

#[test]
fn subledger_mirror_offsets_match_the_real_subledger_layout() {
    assert_eq!(SUB_POS_POOL_OFF, subledger_program::POS_POOL_OFF, "Position.pool offset");
    assert_eq!(SUB_POS_OWNER_OFF, subledger_program::POS_OWNER_OFF, "Position.owner offset");
    assert_eq!(SUB_POS_PRINCIPAL_OFF, subledger_program::POS_PRINCIPAL_OFF, "Position.principal (vote weight) offset");
    assert_eq!(
        SUB_POS_START_SLOT_OFF,
        subledger_program::POS_START_SLOT_OFF,
        "Position.start_slot (vote authorization) offset"
    );
    assert_eq!(
        SUB_POS_ACTION_NONCE_OFF,
        subledger_program::POS_ACTION_NONCE_OFF,
        "Position.action_nonce (vote authorization) offset"
    );
    assert_eq!(
        SUB_POOL_OUTSTANDING_OFF, subledger_program::POOL_OUTSTANDING_PRINCIPAL_OFF,
        "Pool.outstanding_principal (quorum denominator) offset"
    );
    assert_eq!(
        SUB_POOL_DEPOSIT_DEADLINE_OFF,
        subledger_program::POOL_DEPOSIT_DEADLINE_SLOT_OFF,
        "Pool.deposit_deadline_slot offset"
    );
    assert_eq!(
        SUB_POOL_DEPOSIT_WINDOW_OFF,
        subledger_program::POOL_DEPOSIT_WINDOW_SLOTS_OFF,
        "Pool.deposit_window_slots (terminal trigger grace) offset"
    );
    assert_eq!(
        SUB_POOL_DEPOSIT_START_OFF,
        subledger_program::POOL_DEPOSIT_START_SLOT_OFF,
        "Pool.deposit_start_slot offset"
    );
    assert_eq!(
        SUB_POOL_BOOTSTRAP_DELAY_OFF,
        subledger_program::POOL_BOOTSTRAP_DELAY_SLOTS_OFF,
        "Pool.bootstrap_delay_slots offset"
    );
}

#[test]
fn pinned_distribution_program_id_matches_the_real_program() {
    // Finding IC: gv hardcodes the canonical distribution program id (the distribution crate is only a
    // dev-dependency). This catches a typo in that literal against the actually-deployed program.
    assert_eq!(
        genesis_vote_program::DISTRIBUTION_PROGRAM_ID,
        distribution_program::id(),
        "gv's pinned distribution program id must equal the deployed distribution program"
    );
}

#[test]
fn distribution_total_supply_offset_matches_the_real_layout() {
    assert_eq!(
        genesis_vote_program::DIST_CONFIG_TOTAL_SUPPLY_OFF,
        distribution_program::CONFIG_TOTAL_SUPPLY_OFF,
        "Genesis full-allocation registration must read Distribution.total_supply"
    );
}
