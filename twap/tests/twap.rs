// Faithful port of the TWAP buy/burn unit tests from the reference repo
// (percolator-genesis `tests/genesis.rs`). These exercise ONLY the pure ported
// logic: the TWAP schedule, Market0Insurance/Market0Surplus floor protection,
// BidBook ranking/eviction, and execute/plan_execution partial fills. No litesvm
// or percolator-prog harness is involved. Test bodies and asserted numbers are
// copied byte-for-byte from the reference; only the crate import path was changed
// from `percolator_genesis::` to `twap::`.

use solana_program::pubkey::Pubkey;
use twap::percolator_v16::{encode_withdraw_insurance_domain, IX_WITHDRAW_INSURANCE_DOMAIN};
use twap::surplus::{
    derive_associated_token_address, derive_bid_escrow_pda, BidBook, BidRefundAtaSnapshot,
    Market0Insurance, PermissionlessBuyBurnRequest, RegisteredBid, SurplusError,
    TwapAuthorityChain, TwapBuyBurnSchedule, TwapBuyBurnState, TwapProgramConfig,
    TwapWithdrawAccounts, MARKET_0_SURPLUS_BUY_BURN_BPS, MAX_TWAP_BIDS_PER_EXECUTION,
    MAX_TWAP_INTERVAL_COUNT, TWAP_AUTHORITY_SEED, TWAP_INTERVAL_COUNT, TWAP_INTERVAL_SLOTS,
    TWAP_TOTAL_SLOTS,
};
use twap::twap_program::ReusableTwapProgram;

fn key() -> Pubkey {
    Pubkey::new_unique()
}

fn insurance_with_post_genesis_profit(
    insurance_balance_at_handoff: u128,
    reserved_principal: u128,
    post_genesis_profit: u128,
) -> Market0Insurance {
    let mut insurance =
        Market0Insurance::new(insurance_balance_at_handoff, reserved_principal).unwrap();
    insurance.record_profit(post_genesis_profit).unwrap();
    insurance
}

fn escrow_ata(
    bidder: Pubkey,
    coin_mint: Pubkey,
    token_program: Pubkey,
    twap_program: Pubkey,
) -> BidRefundAtaSnapshot {
    BidRefundAtaSnapshot::valid_for(bidder, coin_mint, token_program, twap_program).unwrap()
}

fn full_bid_book_with_target_bid(
    coin_mint: Pubkey,
    token_program: Pubkey,
    twap_program: Pubkey,
    target_bidder: Pubkey,
) -> BidBook {
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    book.place_bid(
        RegisteredBid::pending_coin_for_usdc(target_bidder, 10, 10).unwrap(),
        target_bidder,
        escrow_ata(target_bidder, coin_mint, token_program, twap_program),
        None,
    )
    .unwrap();
    for _ in 1..MAX_TWAP_BIDS_PER_EXECUTION {
        let bidder = key();
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1).unwrap(),
            bidder,
            escrow_ata(bidder, coin_mint, token_program, twap_program),
            None,
        )
        .unwrap();
    }
    book
}

#[test]
fn market0_surplus_twap_pulls_insurance_and_swaps_registered_bids() {
    let mut insurance = insurance_with_post_genesis_profit(200, 0, 400);
    assert_eq!(insurance.expected_floor().unwrap(), 280);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 320);
    let schedule = TwapBuyBurnSchedule::new_interval(100, 50, 2, insurance).unwrap();
    assert_eq!(schedule.total_budget, 320);
    assert_eq!(schedule.budget_released_by(100).unwrap(), 0);
    assert_eq!(schedule.budget_released_by(150).unwrap(), 160);
    assert_eq!(schedule.budget_released_by(200).unwrap(), 320);

    let bidder_a = key();
    let bidder_b = key();
    let bidder_c = key();
    let mut bids = [
        RegisteredBid::pending_coin_for_usdc(bidder_a, 300, 100).unwrap(),
        RegisteredBid::pending_coin_for_usdc(bidder_b, 200, 200).unwrap(),
        RegisteredBid::pending_coin_for_usdc(bidder_c, 50, 50).unwrap(),
    ];
    let mut state = TwapBuyBurnState::new(schedule);

    let first = state.execute(&mut insurance, &mut bids, 150).unwrap();
    assert_eq!(first.pulled_from_insurance, 160);
    assert_eq!(first.burned_coin_atoms, 360);
    assert_eq!(first.fills.len(), 2);
    assert_eq!(first.fills[0].bidder, bidder_a);
    assert_eq!(first.fills[0].usdc_atoms, 100);
    assert_eq!(first.fills[0].coin_atoms, 300);
    assert_eq!(first.fills[1].bidder, bidder_b);
    assert_eq!(first.fills[1].usdc_atoms, 60);
    assert_eq!(first.fills[1].coin_atoms, 60);
    assert_eq!(insurance.insurance_balance, 440);
    assert_eq!(state.pulled_from_insurance, 160);
    assert_eq!(state.burned_coin_atoms, 360);

    let second = state.execute(&mut insurance, &mut bids, 200).unwrap();
    assert_eq!(second.pulled_from_insurance, 160);
    assert_eq!(second.burned_coin_atoms, 160);
    assert_eq!(second.fills.len(), 2);
    assert_eq!(bids[0].remaining_usdc().unwrap(), 0);
    assert_eq!(bids[1].remaining_usdc().unwrap(), 0);
    assert_eq!(bids[2].remaining_usdc().unwrap(), 30);
    assert_eq!(insurance.insurance_balance, 280);
    assert_eq!(insurance.expected_floor().unwrap(), 280);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 0);
    assert_eq!(state.pulled_from_insurance, 320);
    assert_eq!(state.burned_coin_atoms, 520);
}

#[test]
fn market0_default_twap_releases_post_handoff_profit_not_genesis_surplus() {
    let mut insurance = Market0Insurance::new(200, 0).unwrap();
    assert_eq!(insurance.snapshot().surplus(), 200);
    assert_eq!(insurance.snapshot().buy_burn_budget().unwrap(), 160);
    assert_eq!(insurance.snapshot().retained_surplus().unwrap(), 40);
    assert_eq!(insurance.expected_floor().unwrap(), 200);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 0);

    let zero_budget = TwapBuyBurnSchedule::market_0_default(10_000, insurance).unwrap();
    assert_eq!(zero_budget.total_budget, 0);

    insurance.record_profit(400).unwrap();
    assert_eq!(insurance.expected_floor().unwrap(), 280);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 320);

    let start = 10_000;
    let schedule = TwapBuyBurnSchedule::market_0_default(start, insurance).unwrap();
    assert_eq!(schedule.total_budget, 320);
    assert_eq!(schedule.interval_slots, TWAP_INTERVAL_SLOTS);
    assert_eq!(schedule.interval_count, TWAP_INTERVAL_COUNT);
    assert_eq!(schedule.end_slot, start + TWAP_TOTAL_SLOTS);

    assert_eq!(schedule.budget_released_by(start).unwrap(), 0);
    assert_eq!(
        schedule
            .budget_released_by(start + TWAP_INTERVAL_SLOTS - 1)
            .unwrap(),
        0
    );
    assert_eq!(
        schedule
            .budget_released_by(start + TWAP_INTERVAL_SLOTS)
            .unwrap(),
        320 / 150
    );
    assert_eq!(
        schedule
            .budget_released_by(start + (TWAP_INTERVAL_SLOTS * 149))
            .unwrap(),
        (320 * 149) / 150
    );
    assert_eq!(schedule.budget_released_by(schedule.end_slot).unwrap(), 320);
}

#[test]
fn market0_twap_rejects_unbounded_interval_counts() {
    let insurance = Market0Insurance::new(1_000, 600).unwrap();

    assert_eq!(
        TwapBuyBurnSchedule::new_interval(0, 1, MAX_TWAP_INTERVAL_COUNT + 1, insurance,)
            .unwrap_err(),
        SurplusError::InvalidTwapWindow
    );
}

#[test]
fn market0_twap_pauses_below_growing_floor_after_market_loss_until_refilled() {
    let mut insurance = insurance_with_post_genesis_profit(400, 0, 400);
    let schedule = TwapBuyBurnSchedule::new_interval(100, 100, 1, insurance).unwrap();
    let mut state = TwapBuyBurnState::new(schedule);
    let mut bids = [RegisteredBid::pending_coin_for_usdc(key(), 500, 500).unwrap()];

    assert_eq!(insurance.expected_floor().unwrap(), 480);
    insurance.record_loss(330).unwrap();
    assert_eq!(insurance.insurance_balance, 470);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 0);

    let paused = state.execute(&mut insurance, &mut bids, 200).unwrap();
    assert_eq!(paused.pulled_from_insurance, 0);
    assert_eq!(paused.burned_coin_atoms, 0);
    assert_eq!(state.pulled_from_insurance, 0);

    insurance.record_profit(10).unwrap();
    assert_eq!(insurance.insurance_balance, 480);
    assert_eq!(insurance.expected_floor().unwrap(), 480);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 0);

    insurance.record_profit(100).unwrap();
    assert_eq!(insurance.insurance_balance, 580);
    assert_eq!(insurance.expected_floor().unwrap(), 500);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 80);

    let resumed = state.execute(&mut insurance, &mut bids, 200).unwrap();
    assert_eq!(resumed.pulled_from_insurance, 80);
    assert_eq!(resumed.burned_coin_atoms, 80);
    assert_eq!(insurance.insurance_balance, 500);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 0);
}

#[test]
fn market0_twap_percentage_is_squads_controlled_and_defaults_to_eighty_percent() {
    let metadao_futarchy = key();
    let squads = key();
    let twap_program = key();
    let market = key();
    let chain = TwapAuthorityChain::new(metadao_futarchy, squads, twap_program, market).unwrap();
    let withdraw_accounts = TwapWithdrawAccounts {
        percolator_program: key(),
        market,
        twap_pda_collateral_token: key(),
        market_vault_token: key(),
        percolator_vault_authority: key(),
        token_program: key(),
    };
    let mut cfg = TwapProgramConfig::initialize(chain, squads, 0, withdraw_accounts).unwrap();
    assert_eq!(cfg.surplus_buy_burn_bps, MARKET_0_SURPLUS_BUY_BURN_BPS);
    assert_eq!(cfg.surplus_buy_burn_bps, 8_000);

    let default_insurance = insurance_with_post_genesis_profit(400, 0, 400);
    assert_eq!(default_insurance.retained_surplus_floor, 480);
    assert_eq!(
        cfg.market_0_default_schedule(0, default_insurance)
            .unwrap()
            .total_budget,
        320
    );

    assert_eq!(
        cfg.reconfigure_surplus_buy_burn_bps(metadao_futarchy, 9_000)
            .unwrap_err(),
        SurplusError::UnauthorizedController,
        "MetaDAO reaches the TWAP program through Squads, not by bypassing it"
    );
    assert_eq!(
        cfg.reconfigure_surplus_buy_burn_bps(squads, 10_001)
            .unwrap_err(),
        SurplusError::InvalidAmount
    );
    assert_eq!(cfg.surplus_buy_burn_bps, 8_000);

    cfg.reconfigure_surplus_buy_burn_bps(squads, 9_000).unwrap();
    let mut mismatched_insurance = Market0Insurance::new(1_000, 600).unwrap();
    let mismatched_schedule =
        TwapBuyBurnSchedule::new_interval(0, 1, 1, mismatched_insurance).unwrap();
    let mut mismatched_state = TwapBuyBurnState::new(mismatched_schedule);
    let mut mismatched_bids = [RegisteredBid::pending_coin_for_usdc(key(), 10, 10).unwrap()];
    assert_eq!(
        cfg.accept_bids_permissionlessly(
            PermissionlessBuyBurnRequest {
                caller: key(),
                slot: 1,
                withdraw_accounts,
                insurance_ledger: None,
            },
            &mut mismatched_state,
            &mut mismatched_insurance,
            &mut mismatched_bids,
        )
        .unwrap_err(),
        SurplusError::TwapPolicyMismatch,
        "permissionless keepers must not bypass the Squads-set TWAP percentage with mismatched state"
    );

    let mut governed_insurance = Market0Insurance::new_with_buy_burn_bps(400, 0, 9_000).unwrap();
    governed_insurance.record_profit(400).unwrap();
    assert_eq!(governed_insurance.retained_surplus_floor, 440);
    assert_eq!(governed_insurance.withdrawable_surplus().unwrap(), 360);
    assert_eq!(
        cfg.market_0_default_schedule(0, governed_insurance)
            .unwrap()
            .total_budget,
        360
    );
}

#[test]
fn market0_governed_floor_retains_configured_profit_share() {
    let mut insurance = Market0Insurance::new_with_buy_burn_bps(100, 0, 9_000).unwrap();
    assert_eq!(insurance.expected_floor().unwrap(), 100);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 0);

    insurance.record_profit(100).unwrap();
    assert_eq!(insurance.insurance_balance, 200);
    assert_eq!(insurance.expected_floor().unwrap(), 110);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 90);

    insurance.record_loss(95).unwrap();
    assert_eq!(insurance.insurance_balance, 105);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 0);

    insurance.record_profit(100).unwrap();
    assert_eq!(insurance.insurance_balance, 205);
    assert_eq!(insurance.expected_floor().unwrap(), 120);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 85);
}

#[test]
fn market0_twap_partial_fill_rounds_in_bidder_favor() {
    let mut insurance = insurance_with_post_genesis_profit(0, 0, 2);
    let schedule = TwapBuyBurnSchedule::new_interval(0, 1, 1, insurance).unwrap();
    assert_eq!(schedule.total_budget, 1);
    let mut state = TwapBuyBurnState::new(schedule);
    let mut bids = [RegisteredBid::pending_coin_for_usdc(key(), 3, 2).unwrap()];

    let fill = state.execute(&mut insurance, &mut bids, 1).unwrap();
    assert_eq!(fill.pulled_from_insurance, 1);
    assert_eq!(fill.burned_coin_atoms, 1);
    assert_eq!(fill.fills[0].usdc_atoms, 1);
    assert_eq!(fill.fills[0].coin_atoms, 1);
    assert_eq!(bids[0].remaining_coin().unwrap(), 2);
    assert_eq!(bids[0].remaining_usdc().unwrap(), 1);
}

#[test]
fn market0_twap_sub_atom_partial_fill_waits_instead_of_overburning_bidder_coin() {
    let mut insurance = insurance_with_post_genesis_profit(0, 0, 2);
    let schedule = TwapBuyBurnSchedule::new_interval(0, 1, 1, insurance).unwrap();
    assert_eq!(schedule.total_budget, 1);
    let mut state = TwapBuyBurnState::new(schedule);
    let mut bids = [RegisteredBid::pending_coin_for_usdc(key(), 1, 100).unwrap()];

    let fill = state.execute(&mut insurance, &mut bids, 1).unwrap();
    assert_eq!(fill.pulled_from_insurance, 0);
    assert_eq!(fill.burned_coin_atoms, 0);
    assert!(fill.fills.is_empty());
    assert_eq!(insurance.insurance_balance, 2);
    assert_eq!(state.pulled_from_insurance, 0);
    assert_eq!(bids[0].remaining_coin().unwrap(), 1);
    assert_eq!(bids[0].remaining_usdc().unwrap(), 100);
}

#[test]
fn market0_twap_rejects_oversized_bids_that_can_overflow_partial_fill_math() {
    assert_eq!(
        RegisteredBid::pending_coin_for_usdc(key(), u128::MAX, u128::MAX).unwrap_err(),
        SurplusError::InvalidBid
    );
}

#[test]
fn market0_twap_unfillable_bid_does_not_pull_insurance_or_mutate_bids() {
    let mut insurance = insurance_with_post_genesis_profit(0, 0, 10);
    let schedule = TwapBuyBurnSchedule::new_interval(0, 1, 1, insurance).unwrap();
    assert_eq!(schedule.total_budget, 8);
    let mut state = TwapBuyBurnState::new(schedule);
    let mut bids = [RegisteredBid {
        bidder: key(),
        coin_atoms: u128::MAX,
        usdc_atoms: u128::MAX,
        filled_usdc_atoms: 0,
        burned_coin_atoms: 0,
    }];

    let execution = state.execute(&mut insurance, &mut bids, 1).unwrap();
    assert_eq!(execution.pulled_from_insurance, 0);
    assert_eq!(execution.burned_coin_atoms, 0);
    assert!(execution.fills.is_empty());
    assert_eq!(insurance.insurance_balance, 10);
    assert_eq!(state.pulled_from_insurance, 0);
    assert_eq!(state.burned_coin_atoms, 0);
    assert_eq!(bids[0].filled_usdc_atoms, 0);
    assert_eq!(bids[0].burned_coin_atoms, 0);
}

#[test]
fn market0_twap_rejects_more_than_top_64_execution_bids() {
    let mut insurance = insurance_with_post_genesis_profit(0, 0, 2);
    let schedule = TwapBuyBurnSchedule::new_interval(0, 1, 1, insurance).unwrap();
    assert_eq!(schedule.total_budget, 1);
    let mut state = TwapBuyBurnState::new(schedule);
    let weak_bid = RegisteredBid::pending_coin_for_usdc(key(), 1, 1).unwrap();
    let mut bids = vec![weak_bid; MAX_TWAP_BIDS_PER_EXECUTION];
    bids.push(RegisteredBid::pending_coin_for_usdc(key(), 2, 1).unwrap());

    assert_eq!(
        state.execute(&mut insurance, &mut bids, 1).unwrap_err(),
        SurplusError::TooManyBids,
        "permissionless execution must never scan an unbounded bid set"
    );
    assert_eq!(insurance.insurance_balance, 2);
    assert_eq!(state.pulled_from_insurance, 0);
    assert_eq!(state.burned_coin_atoms, 0);
    assert!(bids
        .iter()
        .all(|bid| bid.filled_usdc_atoms == 0 && bid.burned_coin_atoms == 0));
}

#[test]
fn market0_bid_book_replaces_weakest_full_slot_and_refunds_evicted_bidder() {
    let coin_mint = key();
    let token_program = key();
    let twap_program = key();
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    let mut weak_bidders = Vec::new();

    for _ in 0..MAX_TWAP_BIDS_PER_EXECUTION {
        let weak_bidder = key();
        weak_bidders.push(weak_bidder);
        let placement = book
            .place_bid(
                RegisteredBid::pending_coin_for_usdc(weak_bidder, 1, 1).unwrap(),
                weak_bidder,
                escrow_ata(weak_bidder, coin_mint, token_program, twap_program),
                None,
            )
            .unwrap();
        assert!(placement.evicted.is_none());
    }
    let evicted_bidder = weak_bidders[0];

    let equal_bidder = key();
    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(equal_bidder, 1, 1).unwrap(),
            equal_bidder,
            escrow_ata(equal_bidder, coin_mint, token_program, twap_program),
            Some(escrow_ata(
                evicted_bidder,
                coin_mint,
                token_program,
                twap_program,
            )),
        )
        .unwrap_err(),
        SurplusError::BidNotCompetitive
    );

    let strong_bidder = key();
    let placement = book
        .place_bid(
            RegisteredBid::pending_coin_for_usdc(strong_bidder, 2, 1).unwrap(),
            strong_bidder,
            escrow_ata(strong_bidder, coin_mint, token_program, twap_program),
            Some(escrow_ata(
                evicted_bidder,
                coin_mint,
                token_program,
                twap_program,
            )),
        )
        .unwrap();
    let evicted = placement.evicted.unwrap();

    assert_eq!(evicted.bidder, evicted_bidder);
    assert_eq!(evicted.refunded_coin_atoms, 1);
    assert_eq!(evicted.burned_coin_atoms, 0);
    assert_eq!(evicted.unfilled_usdc_atoms, 1);
    assert_eq!(
        book.bids[placement.index].unwrap().bid.bidder,
        strong_bidder
    );
    assert_eq!(book.bids[placement.index].unwrap().bid.coin_atoms, 2);
}

#[test]
fn market0_bid_book_requires_valid_refund_ata_at_placement() {
    let coin_mint = key();
    let token_program = key();
    let twap_program = key();
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    let bidder = key();
    let mut corrupt_refund_ata = escrow_ata(bidder, coin_mint, token_program, twap_program);
    corrupt_refund_ata.token_owner = key();

    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1).unwrap(),
            bidder,
            corrupt_refund_ata,
            None,
        )
        .unwrap_err(),
        SurplusError::InvalidRefundAta
    );

    let mut closed_refund_ata = escrow_ata(bidder, coin_mint, token_program, twap_program);
    closed_refund_ata.is_initialized = false;
    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1).unwrap(),
            bidder,
            closed_refund_ata,
            None,
        )
        .unwrap_err(),
        SurplusError::InvalidRefundAta
    );

    let mut frozen_refund_ata = escrow_ata(bidder, coin_mint, token_program, twap_program);
    frozen_refund_ata.is_frozen = true;
    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1).unwrap(),
            bidder,
            frozen_refund_ata,
            None,
        )
        .unwrap_err(),
        SurplusError::InvalidRefundAta
    );
}

#[test]
fn market0_bid_book_placement_requires_bidder_authorization_and_does_not_mutate_on_failure() {
    let coin_mint = key();
    let token_program = key();
    let twap_program = key();
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    let bidder = key();
    let attacker = key();
    let book_before = book;

    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 10, 10).unwrap(),
            attacker,
            escrow_ata(bidder, coin_mint, token_program, twap_program),
            None,
        )
        .unwrap_err(),
        SurplusError::UnauthorizedBidder
    );
    assert_eq!(book, book_before);

    let placement = book
        .place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 10, 10).unwrap(),
            bidder,
            escrow_ata(bidder, coin_mint, token_program, twap_program),
            None,
        )
        .unwrap();
    assert_eq!(placement.index, 0);
    assert_eq!(book.bids[0].unwrap().bid.bidder, bidder);
}

#[test]
fn market0_bid_book_requires_program_owned_user_escrow_ata() {
    let coin_mint = key();
    let token_program = key();
    let twap_program = key();
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    let bidder = key();
    let escrow_owner = derive_bid_escrow_pda(bidder, twap_program);
    let valid_escrow = escrow_ata(bidder, coin_mint, token_program, twap_program);
    assert_eq!(
        valid_escrow.address,
        derive_associated_token_address(escrow_owner, coin_mint, token_program)
    );
    assert_eq!(valid_escrow.token_owner, escrow_owner);
    assert_ne!(valid_escrow.token_owner, bidder);

    let mut user_owned_ata = valid_escrow;
    user_owned_ata.address = derive_associated_token_address(bidder, coin_mint, token_program);
    user_owned_ata.token_owner = bidder;
    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1).unwrap(),
            bidder,
            user_owned_ata,
            None,
        )
        .unwrap_err(),
        SurplusError::InvalidRefundAta
    );

    let mut user_close_authority = valid_escrow;
    user_close_authority.close_authority = Some(bidder);
    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1).unwrap(),
            bidder,
            user_close_authority,
            None,
        )
        .unwrap_err(),
        SurplusError::InvalidRefundAta
    );

    let mut delegated_escrow = valid_escrow;
    delegated_escrow.delegate = Some(bidder);
    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1).unwrap(),
            bidder,
            delegated_escrow,
            None,
        )
        .unwrap_err(),
        SurplusError::InvalidRefundAta
    );
}

#[test]
fn market0_bid_book_close_escrow_errors_while_bid_active_until_user_withdraws() {
    let coin_mint = key();
    let token_program = key();
    let twap_program = key();
    let bidder = key();
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    let escrow = escrow_ata(bidder, coin_mint, token_program, twap_program);

    book.place_bid(
        RegisteredBid::pending_coin_for_usdc(bidder, 10, 10).unwrap(),
        bidder,
        escrow,
        None,
    )
    .unwrap();
    assert_eq!(
        book.close_bid_escrow(bidder, bidder).unwrap_err(),
        SurplusError::ActiveBidExists
    );

    let attacker = key();
    let book_before = book;
    assert_eq!(
        book.withdraw_bid(bidder, attacker, escrow).unwrap_err(),
        SurplusError::UnauthorizedBidder
    );
    assert_eq!(book, book_before);
    assert_eq!(
        book.close_bid_escrow(bidder, attacker).unwrap_err(),
        SurplusError::UnauthorizedBidder
    );
    assert_eq!(book, book_before);

    let withdrawn = book.withdraw_bid(bidder, bidder, escrow).unwrap();
    assert_eq!(withdrawn.bidder, bidder);
    assert_eq!(withdrawn.escrow_ata, escrow.address);
    assert_eq!(withdrawn.withdrawn_coin_atoms, 10);
    assert_eq!(withdrawn.unfilled_usdc_atoms, 10);
    assert!(book.bids.iter().all(Option::is_none));
    assert_eq!(
        book.close_bid_escrow(bidder, bidder).unwrap(),
        escrow.address
    );
}

#[test]
fn market0_bid_book_rejects_multiple_active_bids_for_same_program_escrow() {
    let coin_mint = key();
    let token_program = key();
    let twap_program = key();
    let bidder = key();
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    let escrow = escrow_ata(bidder, coin_mint, token_program, twap_program);

    book.place_bid(
        RegisteredBid::pending_coin_for_usdc(bidder, 10, 10).unwrap(),
        bidder,
        escrow,
        None,
    )
    .unwrap();
    assert_eq!(
        book.place_bid(
            RegisteredBid::pending_coin_for_usdc(bidder, 20, 10).unwrap(),
            bidder,
            escrow,
            None,
        )
        .unwrap_err(),
        SurplusError::ActiveBidExists
    );

    book.withdraw_bid(bidder, bidder, escrow).unwrap();
    book.place_bid(
        RegisteredBid::pending_coin_for_usdc(bidder, 20, 10).unwrap(),
        bidder,
        escrow,
        None,
    )
    .unwrap();
}

#[test]
fn market0_bid_book_replacement_refunds_unburned_coin_from_partial_bid() {
    let coin_mint = key();
    let token_program = key();
    let twap_program = key();
    let mut book = BidBook::new(coin_mint, token_program, twap_program).unwrap();
    let evicted_bidder = key();
    book.bids[0] = Some(
        RegisteredBid {
            bidder: evicted_bidder,
            coin_atoms: 10,
            usdc_atoms: 10,
            filled_usdc_atoms: 4,
            burned_coin_atoms: 4,
        }
        .with_refund_ata(escrow_ata(
            evicted_bidder,
            coin_mint,
            token_program,
            twap_program,
        )),
    );
    for index in 1..MAX_TWAP_BIDS_PER_EXECUTION {
        let bidder = key();
        book.bids[index] = Some(
            RegisteredBid::pending_coin_for_usdc(bidder, 1, 1)
                .unwrap()
                .with_refund_ata(escrow_ata(bidder, coin_mint, token_program, twap_program)),
        );
    }

    let replacement_bidder = key();
    let replacement = book
        .place_bid(
            RegisteredBid::pending_coin_for_usdc(replacement_bidder, 2, 1).unwrap(),
            replacement_bidder,
            escrow_ata(replacement_bidder, coin_mint, token_program, twap_program),
            Some(escrow_ata(
                evicted_bidder,
                coin_mint,
                token_program,
                twap_program,
            )),
        )
        .unwrap();
    let evicted = replacement.evicted.unwrap();

    assert_eq!(replacement.index, 0);
    assert_eq!(evicted.bidder, evicted_bidder);
    assert_eq!(evicted.refunded_coin_atoms, 6);
    assert_eq!(evicted.burned_coin_atoms, 0);
    assert_eq!(evicted.unfilled_usdc_atoms, 6);
}

#[test]
fn market0_bid_book_burns_evicted_bid_when_refund_ata_is_corrupted_after_placement() {
    for attack in [
        "closed",
        "wrong_address",
        "wrong_account_owner",
        "wrong_token_owner",
        "wrong_mint",
        "frozen_right_before_swap",
    ] {
        let coin_mint = key();
        let token_program = key();
        let twap_program = key();
        let evicted_bidder = key();
        let mut book =
            full_bid_book_with_target_bid(coin_mint, token_program, twap_program, evicted_bidder);
        let mut current_evicted_refund_ata =
            escrow_ata(evicted_bidder, coin_mint, token_program, twap_program);

        match attack {
            "closed" => current_evicted_refund_ata.is_initialized = false,
            "wrong_address" => {
                current_evicted_refund_ata.address =
                    escrow_ata(key(), coin_mint, token_program, twap_program).address;
            }
            "wrong_account_owner" => current_evicted_refund_ata.account_owner = key(),
            "wrong_token_owner" => current_evicted_refund_ata.token_owner = key(),
            "wrong_mint" => current_evicted_refund_ata.mint = key(),
            "frozen_right_before_swap" => current_evicted_refund_ata.is_frozen = true,
            _ => unreachable!(),
        }

        let replacement_bidder = key();
        let replacement_result = book.place_bid(
            RegisteredBid::pending_coin_for_usdc(replacement_bidder, 2, 1).unwrap(),
            replacement_bidder,
            escrow_ata(replacement_bidder, coin_mint, token_program, twap_program),
            Some(current_evicted_refund_ata),
        );

        if attack == "wrong_address" {
            assert_eq!(
                replacement_result.unwrap_err(),
                SurplusError::InvalidRefundAta
            );
            assert_eq!(book.bids[0].unwrap().bid.bidder, evicted_bidder);
            continue;
        }

        let replacement = replacement_result.unwrap();
        let evicted = replacement.evicted.unwrap();

        assert_eq!(replacement.index, 0, "{attack}");
        assert_eq!(evicted.bidder, evicted_bidder, "{attack}");
        assert_eq!(evicted.refund_ata, None, "{attack}");
        assert_eq!(evicted.refunded_coin_atoms, 0, "{attack}");
        assert_eq!(evicted.burned_coin_atoms, 10, "{attack}");
        assert_eq!(evicted.unfilled_usdc_atoms, 10, "{attack}");
        assert_eq!(
            book.bids[replacement.index].unwrap().bid.bidder,
            replacement_bidder,
            "{attack}"
        );
    }
}

#[test]
fn market0_twap_skips_unfillable_poison_bids_so_higher_valid_bid_executes() {
    let mut insurance = insurance_with_post_genesis_profit(0, 0, 2);
    let schedule = TwapBuyBurnSchedule::new_interval(0, 1, 1, insurance).unwrap();
    let mut state = TwapBuyBurnState::new(schedule);
    let strong_bidder = key();
    let mut bids = vec![
        RegisteredBid {
            bidder: key(),
            coin_atoms: u128::MAX,
            usdc_atoms: u128::MAX,
            filled_usdc_atoms: 0,
            burned_coin_atoms: 0,
        },
        RegisteredBid {
            bidder: key(),
            coin_atoms: 10,
            usdc_atoms: 10,
            filled_usdc_atoms: 11,
            burned_coin_atoms: 0,
        },
        RegisteredBid::pending_coin_for_usdc(strong_bidder, 2, 1).unwrap(),
    ];

    let execution = state.execute(&mut insurance, &mut bids, 1).unwrap();

    assert_eq!(execution.pulled_from_insurance, 1);
    assert_eq!(execution.burned_coin_atoms, 2);
    assert_eq!(execution.fills.len(), 1);
    assert_eq!(execution.fills[0].bidder, strong_bidder);
    assert_eq!(bids[0].filled_usdc_atoms, 0);
    assert_eq!(bids[0].burned_coin_atoms, 0);
    assert_eq!(bids[1].filled_usdc_atoms, 11);
    assert_eq!(bids[1].burned_coin_atoms, 0);
    assert_eq!(bids[2].filled_usdc_atoms, 1);
    assert_eq!(bids[2].burned_coin_atoms, 2);
}

#[test]
fn market0_twap_waits_for_registered_bid_capacity_before_pulling_insurance() {
    let mut insurance = insurance_with_post_genesis_profit(0, 0, 13);
    let schedule = TwapBuyBurnSchedule::new_interval(10, 10, 1, insurance).unwrap();
    let mut state = TwapBuyBurnState::new(schedule);
    let mut no_bids = [];

    let empty = state.execute(&mut insurance, &mut no_bids, 20).unwrap();
    assert_eq!(empty.pulled_from_insurance, 0);
    assert_eq!(insurance.insurance_balance, 13);
    assert_eq!(state.pulled_from_insurance, 0);

    let mut bids = [RegisteredBid::pending_coin_for_usdc(key(), 10, 10).unwrap()];
    let fill = state.execute(&mut insurance, &mut bids, 20).unwrap();
    assert_eq!(fill.pulled_from_insurance, 10);
    assert_eq!(fill.burned_coin_atoms, 10);
    assert_eq!(insurance.insurance_balance, 3);
    assert_eq!(state.pulled_from_insurance, 10);
}

#[test]
fn reusable_twap_program_executes_permissionless_buy_burn_from_post_handoff_profit() {
    let metadao_futarchy = key();
    let squads = key();
    let twap_program_id = key();
    let market = key();
    let chain = TwapAuthorityChain::new(metadao_futarchy, squads, twap_program_id, market).unwrap();
    let withdraw_accounts = TwapWithdrawAccounts {
        percolator_program: key(),
        market,
        twap_pda_collateral_token: key(),
        market_vault_token: key(),
        percolator_vault_authority: key(),
        token_program: key(),
    };
    let cfg = TwapProgramConfig::initialize(chain, squads, 0, withdraw_accounts).unwrap();
    let mut insurance = insurance_with_post_genesis_profit(200, 0, 9_375);
    assert_eq!(insurance.expected_floor().unwrap(), 2_075);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 7_500);

    let start_slot = 1_000;
    let mut program =
        ReusableTwapProgram::initialize_market_0_default(cfg, start_slot, insurance).unwrap();
    let mut bids = [RegisteredBid::pending_coin_for_usdc(key(), 50, 50).unwrap()];
    let keeper = key();
    let execution = program
        .accept_bids_permissionlessly(
            PermissionlessBuyBurnRequest {
                caller: keeper,
                slot: start_slot + TWAP_INTERVAL_SLOTS,
                withdraw_accounts,
                insurance_ledger: None,
            },
            &mut insurance,
            &mut bids,
        )
        .unwrap();

    assert_eq!(execution.caller, keeper);
    assert_eq!(execution.insurance_operator_pda, chain.twap_pda);
    assert_eq!(execution.buy_burn.pulled_from_insurance, 50);
    assert_eq!(execution.buy_burn.burned_coin_atoms, 50);
    assert_eq!(program.pulled_from_insurance(), 50);
    assert_eq!(program.burned_coin_atoms(), 50);
    assert_eq!(insurance.insurance_balance, 9_525);
    assert_eq!(insurance.withdrawable_surplus().unwrap(), 7_450);
}

#[test]
fn squads_can_rotate_twap_program_out_and_rebind_market0_operator_pda() {
    let metadao_futarchy = key();
    let squads = key();
    let old_twap_program = key();
    let market = key();
    let chain =
        TwapAuthorityChain::new(metadao_futarchy, squads, old_twap_program, market).unwrap();
    let old_twap_pda = chain.twap_pda;
    let old_withdraw_accounts = TwapWithdrawAccounts {
        percolator_program: key(),
        market,
        twap_pda_collateral_token: key(),
        market_vault_token: key(),
        percolator_vault_authority: key(),
        token_program: key(),
    };
    let mut cfg = TwapProgramConfig::initialize(chain, squads, 0, old_withdraw_accounts).unwrap();

    let new_twap_program = key();
    let expected_new_chain =
        TwapAuthorityChain::new(metadao_futarchy, squads, new_twap_program, market).unwrap();
    let new_withdraw_accounts = TwapWithdrawAccounts {
        percolator_program: old_withdraw_accounts.percolator_program,
        market,
        twap_pda_collateral_token: key(),
        market_vault_token: old_withdraw_accounts.market_vault_token,
        percolator_vault_authority: old_withdraw_accounts.percolator_vault_authority,
        token_program: old_withdraw_accounts.token_program,
    };

    assert_eq!(
        cfg.rotate_twap_program(metadao_futarchy, new_twap_program, new_withdraw_accounts)
            .unwrap_err(),
        SurplusError::UnauthorizedController,
        "MetaDAO must rotate through Squads, not bypass it"
    );
    assert_eq!(
        cfg.rotate_twap_program(squads, old_twap_program, new_withdraw_accounts)
            .unwrap_err(),
        SurplusError::TwapProgramUnchanged
    );

    let rotation = cfg
        .rotate_twap_program(squads, new_twap_program, new_withdraw_accounts)
        .unwrap();
    assert_eq!(rotation.market, market);
    assert_eq!(rotation.old_twap_program, old_twap_program);
    assert_eq!(rotation.old_twap_pda, old_twap_pda);
    assert_eq!(rotation.new_twap_program, new_twap_program);
    assert_eq!(rotation.new_twap_pda, expected_new_chain.twap_pda);
    assert_eq!(cfg.authority_chain, expected_new_chain);
    assert_eq!(cfg.withdraw_accounts, new_withdraw_accounts);

    let mut rejected_insurance = Market0Insurance::new(1_000, 600).unwrap();
    let rejected_schedule =
        TwapBuyBurnSchedule::new_interval(100, 100, 1, rejected_insurance).unwrap();
    let mut rejected_state = TwapBuyBurnState::new(rejected_schedule);
    let mut rejected_bids = [RegisteredBid::pending_coin_for_usdc(key(), 50, 50).unwrap()];
    assert_eq!(
        cfg.accept_bids_permissionlessly(
            PermissionlessBuyBurnRequest {
                caller: key(),
                slot: 200,
                withdraw_accounts: old_withdraw_accounts,
                insurance_ledger: None,
            },
            &mut rejected_state,
            &mut rejected_insurance,
            &mut rejected_bids,
        )
        .unwrap_err(),
        SurplusError::WithdrawAccountsMismatch,
        "old TWAP program withdraw path must stop working after rotation"
    );
    assert_eq!(rejected_insurance.insurance_balance, 1_000);
    assert_eq!(rejected_state.pulled_from_insurance, 0);

    let mut insurance = insurance_with_post_genesis_profit(0, 0, 63);
    let schedule = TwapBuyBurnSchedule::new_interval(100, 100, 1, insurance).unwrap();
    let mut state = TwapBuyBurnState::new(schedule);
    let mut bids = [RegisteredBid::pending_coin_for_usdc(key(), 50, 50).unwrap()];
    let execution = cfg
        .accept_bids_permissionlessly(
            PermissionlessBuyBurnRequest {
                caller: key(),
                slot: 200,
                withdraw_accounts: new_withdraw_accounts,
                insurance_ledger: None,
            },
            &mut state,
            &mut insurance,
            &mut bids,
        )
        .unwrap();

    assert_eq!(
        execution.insurance_operator_pda,
        expected_new_chain.twap_pda
    );
    let withdraw_ix = execution.withdraw_ix.unwrap();
    assert_eq!(withdraw_ix.accounts[0].pubkey, expected_new_chain.twap_pda);
    assert!(withdraw_ix.accounts[0].is_signer);
    assert_ne!(withdraw_ix.accounts[0].pubkey, old_twap_pda);
    assert_eq!(withdraw_ix.accounts[1].pubkey, market);
}

#[test]
fn twap_authority_chain_routes_futarchy_through_squads_to_pda_for_permissionless_bids() {
    let metadao_futarchy = key();
    let squads = key();
    let twap_program = key();
    let market = key();
    let chain = TwapAuthorityChain::new(metadao_futarchy, squads, twap_program, market).unwrap();
    let (expected_pda, expected_bump) =
        Pubkey::find_program_address(&[TWAP_AUTHORITY_SEED, market.as_ref()], &twap_program);
    assert_eq!(chain.twap_pda, expected_pda);
    assert_eq!(chain.bump, expected_bump);
    assert_ne!(chain.twap_pda, metadao_futarchy);
    assert_ne!(chain.twap_pda, squads);

    let percolator_program = key();
    let ledger = key();
    let withdraw_accounts = TwapWithdrawAccounts {
        percolator_program,
        market,
        twap_pda_collateral_token: key(),
        market_vault_token: key(),
        percolator_vault_authority: key(),
        token_program: key(),
    };

    assert_eq!(
        TwapProgramConfig::initialize(chain, metadao_futarchy, 0, withdraw_accounts).unwrap_err(),
        SurplusError::UnauthorizedController
    );
    let mut cfg = TwapProgramConfig::initialize(chain, squads, 0, withdraw_accounts).unwrap();
    assert_eq!(
        cfg.reconfigure_domain(metadao_futarchy, 1).unwrap_err(),
        SurplusError::UnauthorizedController
    );
    cfg.reconfigure_domain(squads, 1).unwrap();
    assert_eq!(cfg.market_0_domain, 1);
    assert_eq!(
        cfg.reconfigure_domain(squads, 2).unwrap_err(),
        SurplusError::InvalidMarket0Domain
    );
    cfg.reconfigure_domain(squads, 0).unwrap();

    let mut bad_withdraw_accounts = withdraw_accounts;
    bad_withdraw_accounts.percolator_program = key();
    let mut rejected_insurance = Market0Insurance::new(1_000, 600).unwrap();
    let rejected_schedule =
        TwapBuyBurnSchedule::new_interval(100, 100, 1, rejected_insurance).unwrap();
    let mut rejected_state = TwapBuyBurnState::new(rejected_schedule);
    let mut rejected_bids = [RegisteredBid::pending_coin_for_usdc(key(), 50, 50).unwrap()];
    assert_eq!(
        cfg.accept_bids_permissionlessly(
            PermissionlessBuyBurnRequest {
                caller: key(),
                slot: 200,
                withdraw_accounts: bad_withdraw_accounts,
                insurance_ledger: Some(ledger),
            },
            &mut rejected_state,
            &mut rejected_insurance,
            &mut rejected_bids,
        )
        .unwrap_err(),
        SurplusError::WithdrawAccountsMismatch
    );
    assert_eq!(rejected_insurance.insurance_balance, 1_000);
    assert_eq!(rejected_state.pulled_from_insurance, 0);
    assert_eq!(rejected_bids[0].remaining_usdc().unwrap(), 50);

    let mut insurance = insurance_with_post_genesis_profit(0, 0, 63);
    let schedule = TwapBuyBurnSchedule::new_interval(100, 100, 1, insurance).unwrap();
    let mut state = TwapBuyBurnState::new(schedule);
    let mut bids = [RegisteredBid::pending_coin_for_usdc(key(), 50, 50).unwrap()];
    let keeper = key();

    let execution = cfg
        .accept_bids_permissionlessly(
            PermissionlessBuyBurnRequest {
                caller: keeper,
                slot: 200,
                withdraw_accounts,
                insurance_ledger: Some(ledger),
            },
            &mut state,
            &mut insurance,
            &mut bids,
        )
        .unwrap();
    assert_eq!(execution.caller, keeper);
    assert_eq!(execution.insurance_operator_pda, chain.twap_pda);
    assert_eq!(execution.buy_burn.pulled_from_insurance, 50);
    assert_eq!(execution.buy_burn.burned_coin_atoms, 50);

    let withdraw_ix = execution.withdraw_ix.unwrap();
    assert_eq!(withdraw_ix.program_id, percolator_program);
    assert_eq!(withdraw_ix.accounts[0].pubkey, chain.twap_pda);
    assert!(withdraw_ix.accounts[0].is_signer);
    assert_ne!(withdraw_ix.accounts[0].pubkey, metadao_futarchy);
    assert_ne!(withdraw_ix.accounts[0].pubkey, squads);
    assert_ne!(withdraw_ix.accounts[0].pubkey, keeper);
    assert_eq!(withdraw_ix.accounts[1].pubkey, market);
    assert_eq!(
        withdraw_ix.accounts[2].pubkey,
        withdraw_accounts.twap_pda_collateral_token
    );
    assert_eq!(withdraw_ix.accounts[6].pubkey, ledger);
    assert_eq!(withdraw_ix.data, encode_withdraw_insurance_domain(0, 50));
    assert_eq!(withdraw_ix.data[0], IX_WITHDRAW_INSURANCE_DOMAIN);
}
