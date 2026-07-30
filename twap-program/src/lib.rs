//! Market-0 TWAP buy/burn program: the post-genesis asset-custody link.
//!
//!   owner-bound subledger pool -> THIS program -> Percolator asset-0 insurance
//!   DAO -> Squads (1-week timelock) -> bounded TWAP policy instructions
//!
//! After the genesis mint, the percolator market-0 insurance authority/operator is
//! rotated from the subledger to this program's `twap_authority` PDA. From then on
//! the TWAP is what touches insurance: it pulls the configured surplus share and
//! (in later slices) buys + burns COIN with it. The TWAP itself is *configured* only
//! by its `squads` controller — a Squads multisig whose `config_authority` is the
//! DAO. Squads never receives a withdrawal-capable role; it can authorize only the
//! fixed policy and custody transitions exposed here. The pull crank is
//! permissionless but bounded by a monotonic principal floor.
//!
//! This slice wires the on-chain keystone: the config that pins the whole chain, and
//! the `twap_authority` PDA signing the percolator insurance CPI. The Squads
//! vault-execute reconfigure path and the COIN buy/burn settlement build on top.
#![allow(clippy::result_large_err)]

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{get_return_data, invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
    sysvar::Sysvar,
};

solana_program::declare_id!("TwapBuyBurn11111111111111111111111111111111");

// The Squads v4 program. The TWAP controller must be a multisig owned by it, so the
// configured controller is provably a real Squads multisig (whose config_authority
// is the DAO) and not an arbitrary key.
const SQUADS_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf");

// Squads v4 `Multisig` account discriminator (anchor account:Multisig). The
// config_authority is at bytes [40..72] of the account.
const SQUADS_MULTISIG_DISC: [u8; 8] = [224, 116, 121, 186, 68, 161, 79, 236];
const SQUADS_PERMISSION_ALL: u8 = 0b111;
const SQUADS_THRESHOLD_OFFSET: usize = 72;
const SQUADS_TIME_LOCK_OFFSET: usize = 74;
const SQUADS_RENT_COLLECTOR_OFFSET: usize = 94;
// The DAO governs this program ONLY through the timelocked Squads multisig — the 1-week delay is
// the depositor-protection window (time to react/exit before any insurance-affecting action lands).
// init_config must REFUSE to bind a multisig whose on-chain `time_lock` is below this, so the
// security model's premise is enforced on-chain instead of trusted to the (off-chain) orchestration.
const MIN_TIMELOCK_SECS: u32 = 7 * 24 * 60 * 60; // 604_800

// Associated Token Account program — used to derive a bidder's CANONICAL COIN ATA as the auction
// refund target. Pinning refunds to the canonical ATA (not an arbitrary caller account) means a
// bidder cannot brick the book by closing the refund destination: anyone can recreate an ATA, so
// a stuck claim is always recoverable (it is not a permanent DOS).
const ATA_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn canonical_token_account(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

fn bidder_coin_ata(bidder: &Pubkey, coin_mint: &Pubkey) -> Pubkey {
    canonical_token_account(bidder, coin_mint)
}

// The twap_authority PDA seed — matches the `twap` lib's TWAP_AUTHORITY_SEED so the
// authority address is the canonical market-0 TWAP authority.
const TWAP_AUTHORITY_SEED: &[u8] = b"market-0-twap";
const CONFIG_SEED: &[u8] = b"twap_config";

const CONFIG_DISC: [u8; 8] = *b"TWAPCFG1";
const INITIAL_CONFIG_SIZE: usize = 200;
const LEGACY_CONFIG_SIZE: usize = 232;
const CUSTODY_CONFIG_SIZE: usize = 264;
const PROVENANCE_CONFIG_SIZE: usize = 272;
const CONFIG_SIZE: usize = 288;
const CUSTODY_MODE_UNATTESTED: u8 = 0;
const CUSTODY_MODE_POOL_BOUND: u8 = 1;
const CUSTODY_MODE_POOLLESS_EMPTY: u8 = 2;

// Default surplus share routed to buy/burn (the rest is retained as insurance).
const DEFAULT_SURPLUS_BUY_BURN_BPS: u16 = 8_000;
const BPS_DENOMINATOR: u16 = 10_000;
const REMAINDER_RADIX: u64 = BPS_DENOMINATOR as u64;
const REMAINDER_RADIX_SQUARED: u64 = REMAINDER_RADIX * REMAINDER_RADIX;
const PACKED_REMAINDER_LIMIT: u64 = REMAINDER_RADIX_SQUARED * REMAINDER_RADIX;

// Percolator CPI tags (verified against the pinned real v16 program).
// tag 57 = WithdrawInsuranceAsset { asset_index: u16, amount: u128 } — the consolidated, asset-indexed,
// insurance-operator-gated, during-Live (mode==0) insurance withdraw that REPLACED the removed asset-0
// tag-23 WithdrawInsuranceLimited (reconcile, finding JX). Accounts: [operator(s), market(w), dest(w),
// vault(w), vault_authority, token_program, ledger(optional)] — same order the old tag-23 pull used.
const PERC_IX_WITHDRAW_INSURANCE_ASSET: u8 = 57;
const PERC_IX_TOP_UP_INSURANCE: u8 = 9;
const PERC_IX_TOP_UP_INSURANCE_DOMAIN: u8 = 56;
const PERC_IX_UPDATE_BACKING_FEE_POLICY: u8 = 51;
const PERC_IX_UPDATE_TRADE_FEE_POLICY: u8 = 55;
const PERC_IX_UPDATE_ASSET_AUTHORITY: u8 = 65;
const PERC_IX_RESTART_ASSET_ORACLE: u8 = 69;
const ASSET_AUTH_ADMIN: u8 = 0;
const ASSET_AUTH_INSURANCE: u8 = 1; // insurance_authority (gates TopUpInsurance / deposits)
const ASSET_AUTH_INSURANCE_OPERATOR: u8 = 2;
const SUBLEDGER_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("Sub1edger1111111111111111111111111111111111");
const SUBLEDGER_IX_ACCEPT_OPERATOR: u8 = 7;
const SUBLEDGER_IX_ASSERT_NO_PRINCIPAL: u8 = 10;
const SUBLEDGER_IX_ASSERT_PRINCIPAL: u8 = 11;
const SUBLEDGER_IX_INSURANCE_WITHDRAW_FULL: u8 = 13;
const SUBLEDGER_IX_PREPARE_ASSET0_RESTART: u8 = 16;
const RESTART_CHECKPOINT_RETURN_DISC: [u8; 8] = *b"RSTFLR01";
const MARKET_CONTROLLER_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("3ueoyr1JepT2DvPxh8LrhdJZ6YsL2sT9Sm7y3TfNyfi9");
const MARKET_CONTROLLER_SEED: &[u8] = b"market-controller";
const MARKET_CONTROLLER_IX_RETURN_RESOLVED_ASSET0_BACKING: u8 = 7;

const IX_INIT_CONFIG: u8 = 0;
// Reconfigure the surplus buy/burn share. Gated on the Squads VAULT PDA, which can
// only sign via a multisig vault-transaction execute — i.e. after a DAO proposal
// clears the 1-week Squads timelock. This is the on-chain Squads -> TWAP control.
const IX_RECONFIGURE: u8 = 2;
// Legacy direct handoff for an unfunded market whose Squads vault is still the raw
// asset admin. Funded genesis uses IX_ACCEPT_FROM_SUBLEDGER.
const IX_ACCEPT_OPERATOR: u8 = 3;
// Raise the surplus floor after its initial value. Canonical funded handoff derives
// the initial minimum directly from the source pool's outstanding principal.
const IX_SET_RESERVED_FLOOR: u8 = 4;
// Create the buy/burn AuctionBook + its shared COIN escrow / settlement-USD token accounts.
// Squads-vault-gated (timelock'd) — pins the reserve, round length, COIN sink and binding mints.
// Everything that drives the auction afterwards is permissionless.
const IX_INIT_BOOK: u8 = 5;
// Update the reserve rate (the max USD-per-COIN the protocol will pay). Squads-vault-gated.
const IX_SET_RESERVE: u8 = 6;
// Place a bid: PERMISSIONLESS. The bidder escrows COIN and offers it for USD at a limit rate.
// Once placed a bid CANNOT be cancelled (anti-spoofing — a spoofer must not be able to yank a
// bid right before execution). It only leaves the book early by being evicted by a STRICTLY
// better bid (which refunds it). This is the deliberate fix vs the twap lib's withdraw_bid.
const IX_PLACE_BID: u8 = 7;
// Execute the auction: PERMISSIONLESS, allowed once the round's slots have expired. The SOLE path
// that moves insurance: it pulls the burn-share of the current percolator surplus as the auction
// budget, ratchets the retained share into the principal counter, clears the whole book at one
// marginal uniform (Dutch) price, and burns OR sends the bought COIN. Then a new round opens.
const IX_EXECUTE: u8 = 8;
// Claim a settled bid: PERMISSIONLESS, per bid. Pays the bid's won USD and refunds any
// unsold/over-escrowed COIN, then frees the slot.
const IX_CLAIM: u8 = 9;
// Set the COIN sink: futarchy-configurable whether bought COIN is BURNED or SENT to an account
// (e.g. a DAO treasury). Squads-vault-gated.
const IX_SET_COIN_SINK: u8 = 10;
// Shutdown / wind-down: sweep the TWAP's accumulated USD budget (the unspent dollars in the
// holding) to a DAO-supplied address. The TWAP normally KEEPS its dollars across rounds and adds
// more each execute; this Squads-vault-gated path is the only way to take them back.
const IX_SHUTDOWN: u8 = 11;
// Set the flat per-bid COIN fee (burned on every place_bid to deter spam). Squads-vault-gated.
const IX_SET_BID_FEE: u8 = 12;
// Cancel an unsettled bid and reclaim its escrowed COIN — bidder-signed, allowed only AFTER an
// execute has cleared the book once (one round) or 2*round_length slots have passed since
// placement. The cooldown removes the last-second cancel that could otherwise manipulate a pending
// execute (no race); a settled bid uses `claim` instead.
const IX_CANCEL_BID: u8 = 13;
// Set the 4-way surplus economics: base_unit_savings_bps (surplus withdrawn to the savings sink) and
// buyback_bps (of the auction's bought COIN, the fraction retained to the sink instead of burned), plus
// the savings sink account. Squads-vault-gated. Validates auction + savings <= 100% (so insurance growth
// stays >= 0 and the savings withdraw can never reach principal) and buyback <= auction.
const IX_SET_ECONOMICS: u8 = 14;
// Fixed pool -> TWAP custody transition, called only by the subledger pool PDA.
const IX_ACCEPT_FROM_SUBLEDGER: u8 = 15;
// Governance-authorized live recovery transition back to the owner-bound subledger pool;
// permissionless once the market is resolved/empty while owner principal remains.
const IX_RETURN_TO_SUBLEDGER: u8 = 16;
// Permissionless inbound-only donation: donor -> TWAP holding -> Percolator insurance.
const IX_DONATE_INSURANCE: u8 = 17;
// Timelocked, fee-only Percolator policy update; no value accounts are accepted.
const IX_SET_MARKET_FEES: u8 = 18;
// Permissionless after resolution and after the bound pool proves all owner
// principal is gone. Routes protocol insurance to the bound Squads vault.
const IX_RETURN_RESOLVED_PROTOCOL_INSURANCE: u8 = 19;
// Permissionless, amountless wrapper for the controller's provider-bound asset-0
// backing return while a pool-less config's TWAP PDA remains asset_admin.
const IX_RETURN_RESOLVED_ASSET0_BACKING: u8 = 20;
// Timelocked, value-neutral restart for asset 0 while this program holds asset_admin.
const IX_RESTART_ASSET0: u8 = 21;
// Fixed ingress for Subledger-derived cross-backing earnings. The bound pool
// signs and supplies the exact amount; this program fixes the recipient owner to
// the config's Squads vault and never accepts backing principal directly.
const IX_ACCEPT_CROSS_BACKING_EARNINGS: u8 = 22;
const SUBLEDGER_POOL_FLAGS_OFF: usize = 272;
const SUBLEDGER_POOL_FLAG_CROSS_BACKING: u8 = 1 << 0;

// spl-token instruction tags used in CPIs we build by hand (avoids pulling spl's ix builders
// into the BPF object, and keeps the data shape explicit).
const TOKEN_IX_TRANSFER: u8 = 3;
const TOKEN_IX_BURN: u8 = 8;

// The auction book is a single account per config (one live auction per market-0 twap). Its
// shared COIN escrow + settlement-USD accounts are owned by a book-escrow PDA so execution
// burns/pays from one place and the book tracks per-bid shares.
const BOOK_SEED: &[u8] = b"twap_book";
const BOOK_ESCROW_SEED: &[u8] = b"twap_book_escrow";
const BOOK_DISC: [u8; 8] = *b"TWAPBOK1";
// Bids in the book. 32 bounds the O(N^2) ranking compute and the account size (~5KB).
const MAX_BIDS: usize = 32;
const BOOK_STATE_OPEN: u8 = 0;
const BOOK_STATE_SETTLED: u8 = 1;
// COIN sink modes (what to do with the bought COIN): 0 = burn (default), 1 = send to an account.
const SINK_SEND: u8 = 1;
// Round-not-expired custom error for execute.
const ERR_ROUND_ACTIVE: u32 = 1;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

// The config PDA commits to ALL caller-supplied bindings, not just the market. Keying
// it on market alone made init_config (which is permissionless) front-run squattable:
// an attacker could stand up a throwaway Squads multisig (config_authority = itself),
// pass the internal consistency check, and init the per-market config first with their
// own bindings — permanently blocking the real DAO's deployment for that market (the
// squatted config is inert, but the PDA is taken and cannot be re-initialized). By
// folding squads_multisig + coin_mint + percolator_program into the seed, the only
// config that can exist at the legit address is one carrying the legit bindings (which
// in turn forces the real metadao_futarchy via the config_authority check) — so a
// front-run at that address merely reproduces the correct config and does no harm; any
// attacker variation lands at a different PDA the real deployment ignores. (finding P)
fn config_seeds<'a>(
    market: &'a Pubkey,
    squads_multisig: &'a Pubkey,
    coin_mint: &'a Pubkey,
    percolator_program: &'a Pubkey,
) -> [&'a [u8]; 5] {
    [
        CONFIG_SEED,
        market.as_ref(),
        squads_multisig.as_ref(),
        coin_mint.as_ref(),
        percolator_program.as_ref(),
    ]
}

// The twap_authority is the percolator insurance OPERATOR granted by the handoff, and it signs
// WithdrawInsuranceLimited. Its seed MUST commit to the whole config, not just (market, perc):
// `execute` computes the pull as `insurance - config.reserved_floor` using the CALLING config's own
// floor. If the seed were only (market, percolator_program), two configs on the SAME market+perc
// (differing only in squads/coin) would share ONE operator PDA — so an attacker could stand up a
// PARASITE config on the victim's market, set ITS OWN reserved_floor to 0, and crank execute to pull
// the victim's entire insurance (principal included) into the parasite's holding, since percolator
// only checks that the signer is the (shared) operator. Binding to the config PDA (which finding P
// already commits to market+squads+coin+perc) makes the operator UNIQUE per config: only the single
// config the handoff actually granted to derives the real operator; any parasite derives a powerless
// PDA percolator does not recognize. (finding AD — original perc-binding; finding AQ — config-binding)
fn authority_seeds<'a>(config: &'a Pubkey) -> [&'a [u8]; 2] {
    [TWAP_AUTHORITY_SEED, config.as_ref()]
}

// The Squads multisig's default (index 0) vault PDA — the address that signs the
// inner instructions of an executed multisig vault-transaction.
fn squads_default_vault(multisig: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"multisig", multisig.as_ref(), b"vault", &[0u8]],
        &SQUADS_PROGRAM_ID,
    )
    .0
}

fn perc_vault_authority(market_slab: &Pubkey, percolator_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market_slab.as_ref()], percolator_program).0
}

/// Create a program-owned PDA, tolerating an attacker pre-funding the (deterministic) address.
/// System `create_account` aborts with AccountAlreadyInUse on ANY pre-existing lamports, so a 1-
/// lamport transfer to the address — which needs no signature — would PERMANENTLY brick init (the
/// lamports can never be swept from a system-owned PDA). Instead top up the rent shortfall (a plain
/// transfer) then allocate + assign via invoke_signed; both only require the account to be data-empty
/// + system-owned, true for a merely pre-funded address. Callers gate re-init on `data_len() != 0`
/// (NOT `lamports() != 0`). (finding AI)
fn create_pda_robust<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    program_id: &Pubkey,
    seeds: &[&[u8]],
    size: usize,
) -> ProgramResult {
    let rent = solana_program::rent::Rent::get()?;
    let required = rent.minimum_balance(size);
    let current = account.lamports();
    if current < required {
        invoke(
            &system_instruction::transfer(payer.key, account.key, required - current),
            &[payer.clone(), account.clone(), system_program.clone()],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(account.key, size as u64),
        &[account.clone(), system_program.clone()],
        &[seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(account.key, program_id),
        &[account.clone(), system_program.clone()],
        &[seeds],
    )?;
    Ok(())
}

// Parse the stable Squads v4 Multisig prefix plus its Borsh Option<Pubkey> and member vector.
// `config_authority` alone does not authenticate transaction control: Squads creation does not
// require that key to sign, so anyone can name MetaDAO there while installing themselves as the
// voting member. This program intentionally accepts only the minimal timelock wrapper used by the
// protocol: 1-of-1, with MetaDAO as the sole initiate/vote/execute member.
fn validate_metadao_squads_control(data: &[u8], metadao: &Pubkey) -> ProgramResult {
    if data.len() <= SQUADS_RENT_COLLECTOR_OFFSET
        || data[..8] != SQUADS_MULTISIG_DISC
        || u16::from_le_bytes(
            data[SQUADS_THRESHOLD_OFFSET..SQUADS_THRESHOLD_OFFSET + 2]
                .try_into()
                .unwrap(),
        ) != 1
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let mut offset = SQUADS_RENT_COLLECTOR_OFFSET;
    offset = match data[offset] {
        0 => offset + 1,
        1 => offset
            .checked_add(1 + 32)
            .ok_or(ProgramError::InvalidAccountData)?,
        _ => return Err(ProgramError::InvalidAccountData),
    };
    // bump (u8), then members Vec length (u32), then one Member { key, permissions }.
    let member_count_offset = offset
        .checked_add(1)
        .ok_or(ProgramError::InvalidAccountData)?;
    let member_offset = member_count_offset
        .checked_add(4)
        .ok_or(ProgramError::InvalidAccountData)?;
    let member_end = member_offset
        .checked_add(33)
        .ok_or(ProgramError::InvalidAccountData)?;
    if member_end > data.len()
        || u32::from_le_bytes(data[member_count_offset..member_offset].try_into().unwrap()) != 1
        || Pubkey::new_from_array(data[member_offset..member_offset + 32].try_into().unwrap())
            != *metadao
        || data[member_offset + 32] != SQUADS_PERMISSION_ALL
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

// Shared offsets are derived from the exact Cargo-pinned engine structs. The LiteSVM canary
// also pins the wrapper-owned prefix against the real pinned Percolator program.
pub const PERC_MARKET_GROUP_OFFSET: usize = percolator_accounting::MARKET_GROUP_OFFSET;
pub const INSURANCE_OFFSET: usize = percolator_accounting::INSURANCE_OFFSET;

/// Read one asset's domain-budget remainder. The header insurance value is market-wide
/// and cannot safely price an asset-local tag-57 withdrawal.
fn read_asset_insurance(slab_data: &[u8], asset_index: usize) -> Result<u128, ProgramError> {
    percolator_accounting::read_asset_insurance_remaining(slab_data, asset_index)
        .map_err(|_| ProgramError::InvalidAccountData)
}

fn read_asset_insurance_spent_total(
    slab_data: &[u8],
    asset_index: usize,
) -> Result<u128, ProgramError> {
    percolator_accounting::read_asset_insurance_spent(slab_data, asset_index)
        .map_err(|_| ProgramError::InvalidAccountData)?
        .into_iter()
        .try_fold(0u128, |total, spent| total.checked_add(spent))
        .ok_or(ProgramError::ArithmeticOverflow)
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct Config {
    coin_mint: Pubkey,
    market_slab: Pubkey,
    percolator_program: Pubkey,
    /// The Squads multisig that controls (reconfigures/rotates) this TWAP. Its
    /// `config_authority` is the DAO, so the DAO governs the TWAP only via Squads.
    squads_multisig: Pubkey,
    /// The winning genesis DAO (metadao futarchy authority).
    metadao_futarchy: Pubkey,
    /// Share of each round's surplus routed to the buy auction (burn + buyback combined).
    /// The remainder splits between base-unit savings (below) and insurance growth.
    surplus_buy_burn_bps: u16,
    market_0_domain: u8,
    config_bump: u8,
    authority_bump: u8,
    /// The asset-0 insurance amount that `execute` must NEVER pull below — the reserved
    /// depositor principal (+ any retained buffer). `execute`'s surplus pull may move at most
    /// `insurance - reserved_floor`. Initialized to u128::MAX (no pulls). A canonical
    /// funded handoff replaces the sentinel with at least the source pool's outstanding
    /// principal. The DAO can only raise it; a fixed recovery/re-handoff may replace only
    /// its recorded pool-principal component with the pool's new live principal.
    reserved_floor: u128,
    /// 4-way surplus economics (DAO-tunable, timelock'd). Each round's surplus splits:
    ///   auction     = surplus_buy_burn_bps           -> buy COIN; of the BOUGHT COIN, buyback_bps is
    ///                                                   RETAINED to the book's COIN sink (book.coin_sink,
    ///                                                   set via init_book / set_coin_sink), the rest BURNED.
    ///   savings     = base_unit_savings_bps          -> withdrawn (tag-57) to base_unit_savings_account
    ///                                                   in the asset's base unit (collateral/USD).
    ///   insurance   = 10_000 - auction - savings     -> retained in insurance (ratcheted into the floor).
    /// buyback_bps <= 10_000 (a fraction of bought COIN, post-purchase — never touches principal);
    /// surplus_buy_burn_bps + base_unit_savings_bps <= 10_000 (the floor-protection invariant).
    /// Defaults: savings 0, buyback 0 (= today's burn-only auction + insurance remainder).
    base_unit_savings_bps: u16,
    buyback_bps: u16,
    /// DAO/futarchy-owned account that receives the savings withdraw (a collateral token account) and,
    /// in SEND/buyback mode, the bought-back COIN sink. default() when both shares are 0.
    base_unit_savings_account: Pubkey,
    /// The one owner-bound subledger pool that handed custody to this TWAP. Set
    /// exactly once by the pool -> TWAP transition and used as the only permitted
    /// recovery destination. Legacy unfunded direct migrations leave this unset.
    custody_pool: Pubkey,
    /// Pool-owned insurance included in `reserved_floor` at the most recent custody
    /// handoff. A later recovery/re-handoff may replace only this component with the
    /// pool's new live insurance claim; retained insurance and DAO-raised buffers stay locked.
    custody_principal: u64,
    /// Current-layout custody provenance. Historical zero means no safe inference.
    /// Pool-less terminal recovery requires the explicit empty-at-handoff marker.
    custody_mode: u8,
    /// Set only after an atomic live owner exit returns roles to `custody_pool`.
    /// It authorizes one permissionless re-handoff to this exact config.
    rehandoff_pending: bool,
    /// Fractional basis-point numerator carried between settled rounds. This keeps
    /// public atom-sized fills from repeatedly rounding the reward leg down.
    buyback_remainder_bps: u16,
    /// Fractional basis-point numerator for the combined auction+savings pull. Keeping
    /// one combined carry guarantees the two external routes can never exceed surplus.
    external_surplus_remainder_bps: u16,
    /// Fractional numerator used to apportion each combined pull between auction and
    /// savings. Its denominator is the current auction+savings bps total.
    auction_split_remainder_bps: u16,
    /// Sum of asset-0's two cumulative insurance-spent counters at the most recent
    /// pool handoff. Unlike a balance snapshot, this cannot be raised by later fees
    /// or donations, so recovery can preserve losses that occurred under TWAP custody.
    custody_insurance_spent: u128,
}

impl Config {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if (data.len() != LEGACY_CONFIG_SIZE
            && data.len() != CUSTODY_CONFIG_SIZE
            && data.len() != PROVENANCE_CONFIG_SIZE
            && data.len() < CONFIG_SIZE)
            || data[..8] != CONFIG_DISC
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let custody_mode = if data.len() >= PROVENANCE_CONFIG_SIZE {
            data[265]
        } else {
            0
        };
        let rehandoff_pending = if data.len() >= PROVENANCE_CONFIG_SIZE {
            data[266]
        } else {
            0
        };
        let remainder_offset = match data.len() {
            LEGACY_CONFIG_SIZE => 225,
            CUSTODY_CONFIG_SIZE => 257,
            _ => 267,
        };
        let mut packed_remainders_bytes = [0u8; 8];
        packed_remainders_bytes[..5]
            .copy_from_slice(&data[remainder_offset..remainder_offset + 5]);
        let packed_remainders = u64::from_le_bytes(packed_remainders_bytes);
        // Three base-10_000 digits fit in the five reserved bytes because 10_000^3 < 2^40.
        // The buyback digit stays least-significant so configs written by the predecessor
        // (a plain little-endian u16 followed by zeroes) decode without migration.
        let buyback_remainder_bps = (packed_remainders % REMAINDER_RADIX) as u16;
        let external_surplus_remainder_bps =
            ((packed_remainders / REMAINDER_RADIX) % REMAINDER_RADIX) as u16;
        let auction_split_remainder_bps =
            ((packed_remainders / REMAINDER_RADIX_SQUARED) % REMAINDER_RADIX) as u16;
        if custody_mode > CUSTODY_MODE_POOLLESS_EMPTY
            || rehandoff_pending > 1
            || packed_remainders >= PACKED_REMAINDER_LIMIT
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            market_slab: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            percolator_program: Pubkey::new_from_array(data[72..104].try_into().unwrap()),
            squads_multisig: Pubkey::new_from_array(data[104..136].try_into().unwrap()),
            metadao_futarchy: Pubkey::new_from_array(data[136..168].try_into().unwrap()),
            surplus_buy_burn_bps: u16::from_le_bytes(data[168..170].try_into().unwrap()),
            market_0_domain: data[170],
            config_bump: data[171],
            authority_bump: data[172],
            reserved_floor: u128::from_le_bytes(data[173..189].try_into().unwrap()),
            base_unit_savings_bps: u16::from_le_bytes(data[189..191].try_into().unwrap()),
            buyback_bps: u16::from_le_bytes(data[191..193].try_into().unwrap()),
            base_unit_savings_account: Pubkey::new_from_array(data[193..225].try_into().unwrap()),
            custody_pool: if data.len() >= CUSTODY_CONFIG_SIZE {
                Pubkey::new_from_array(data[225..257].try_into().unwrap())
            } else {
                Pubkey::default()
            },
            custody_principal: if data.len() >= PROVENANCE_CONFIG_SIZE {
                u64::from_le_bytes(data[257..265].try_into().unwrap())
            } else {
                0
            },
            custody_mode,
            rehandoff_pending: rehandoff_pending == 1,
            buyback_remainder_bps,
            external_surplus_remainder_bps,
            auction_split_remainder_bps,
            custody_insurance_spent: if data.len() >= CONFIG_SIZE {
                u128::from_le_bytes(data[272..288].try_into().unwrap())
            } else {
                0
            },
        })
    }

    fn serialize(&self, data: &mut [u8]) -> ProgramResult {
        if data.len() != LEGACY_CONFIG_SIZE
            && data.len() != CUSTODY_CONFIG_SIZE
            && data.len() != PROVENANCE_CONFIG_SIZE
            && data.len() < CONFIG_SIZE
        {
            return Err(ProgramError::InvalidAccountData);
        }
        if self.buyback_remainder_bps >= BPS_DENOMINATOR
            || self.external_surplus_remainder_bps >= BPS_DENOMINATOR
            || self.auction_split_remainder_bps >= BPS_DENOMINATOR
        {
            return Err(ProgramError::InvalidAccountData);
        }
        data[..8].copy_from_slice(&CONFIG_DISC);
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[40..72].copy_from_slice(self.market_slab.as_ref());
        data[72..104].copy_from_slice(self.percolator_program.as_ref());
        data[104..136].copy_from_slice(self.squads_multisig.as_ref());
        data[136..168].copy_from_slice(self.metadao_futarchy.as_ref());
        data[168..170].copy_from_slice(&self.surplus_buy_burn_bps.to_le_bytes());
        data[170] = self.market_0_domain;
        data[171] = self.config_bump;
        data[172] = self.authority_bump;
        data[173..189].copy_from_slice(&self.reserved_floor.to_le_bytes());
        data[189..191].copy_from_slice(&self.base_unit_savings_bps.to_le_bytes());
        data[191..193].copy_from_slice(&self.buyback_bps.to_le_bytes());
        data[193..225].copy_from_slice(self.base_unit_savings_account.as_ref());
        if data.len() >= CUSTODY_CONFIG_SIZE {
            data[225..257].copy_from_slice(self.custody_pool.as_ref());
            if data.len() >= PROVENANCE_CONFIG_SIZE {
                data[257..265].copy_from_slice(&self.custody_principal.to_le_bytes());
                data[265] = self.custody_mode;
                data[266] = self.rehandoff_pending as u8;
            } else {
                data[257..CUSTODY_CONFIG_SIZE].fill(0);
            }
        } else {
            data[227..LEGACY_CONFIG_SIZE].fill(0);
        }
        let remainder_offset = match data.len() {
            LEGACY_CONFIG_SIZE => 225,
            CUSTODY_CONFIG_SIZE => 257,
            _ => 267,
        };
        let packed_remainders = self.buyback_remainder_bps as u64
            + (self.external_surplus_remainder_bps as u64) * REMAINDER_RADIX
            + (self.auction_split_remainder_bps as u64) * REMAINDER_RADIX_SQUARED;
        data[remainder_offset..remainder_offset + 5]
            .copy_from_slice(&packed_remainders.to_le_bytes()[..5]);
        if data.len() >= CONFIG_SIZE {
            data[272..288].copy_from_slice(&self.custody_insurance_spent.to_le_bytes());
        } else if self.custody_insurance_spent != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }
}

// The initial 200-byte config is recognized only by the two fund-release handlers. Its authority
// PDA generations were retired for security reasons, so it must never reach a signer or admin path.
fn validate_exit_config(data: &[u8]) -> ProgramResult {
    if data.len() == INITIAL_CONFIG_SIZE {
        if data[..8] != CONFIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    } else {
        Config::deserialize(data).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let (tag, data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        IX_INIT_CONFIG => process_init_config(program_id, accounts, data),
        IX_RECONFIGURE => process_reconfigure(program_id, accounts, data),
        IX_SET_RESERVED_FLOOR => process_set_reserved_floor(program_id, accounts, data),
        IX_ACCEPT_OPERATOR => process_accept_operator(program_id, accounts, data),
        IX_INIT_BOOK => process_init_book(program_id, accounts, data),
        IX_SET_RESERVE => process_set_reserve(program_id, accounts, data),
        IX_PLACE_BID => process_place_bid(program_id, accounts, data),
        IX_EXECUTE => process_execute(program_id, accounts, data),
        IX_CLAIM => process_claim(program_id, accounts, data),
        IX_SET_COIN_SINK => process_set_coin_sink(program_id, accounts, data),
        IX_SHUTDOWN => process_shutdown(program_id, accounts, data),
        IX_SET_BID_FEE => process_set_bid_fee(program_id, accounts, data),
        IX_CANCEL_BID => process_cancel_bid(program_id, accounts, data),
        IX_SET_ECONOMICS => process_set_economics(program_id, accounts, data),
        IX_ACCEPT_FROM_SUBLEDGER => process_accept_from_subledger(program_id, accounts, data),
        IX_RETURN_TO_SUBLEDGER => process_return_to_subledger(program_id, accounts, data),
        IX_DONATE_INSURANCE => process_donate_insurance(program_id, accounts, data),
        IX_SET_MARKET_FEES => process_set_market_fees(program_id, accounts, data),
        IX_RETURN_RESOLVED_PROTOCOL_INSURANCE => {
            process_return_resolved_protocol_insurance(program_id, accounts, data)
        }
        IX_RETURN_RESOLVED_ASSET0_BACKING => {
            process_return_resolved_asset0_backing(program_id, accounts, data)
        }
        IX_RESTART_ASSET0 => process_restart_asset0(program_id, accounts, data),
        IX_ACCEPT_CROSS_BACKING_EARNINGS => {
            process_accept_cross_backing_earnings(program_id, accounts, data)
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// init_config accounts: [payer(s,w), coin_mint, market_slab, config(pda,w),
//   squads_multisig, metadao_futarchy, percolator_program, system]
//
// Pins the whole authority chain: the controller must be a real Squads multisig and
// the DAO (metadao_futarchy) is recorded. The twap_authority PDA derived here is the
// address that must hold percolator's insurance authority/operator role.
fn process_init_config(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let squads_multisig = next_account_info(iter)?;
    let metadao_futarchy = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    // The controller must be a genuine Squads multisig — that is the only account
    // through which the DAO can ever reach this program.
    if *squads_multisig.owner != SQUADS_PROGRAM_ID {
        return Err(ProgramError::IllegalOwner);
    }
    if *metadao_futarchy.key == Pubkey::default() || *percolator_program.key == Pubkey::default() {
        return Err(ProgramError::InvalidAccountData);
    }
    // The named DAO must be both config authority and the sole all-permissions member.
    // Squads does not require config_authority to sign creation, so that field alone
    // is not proof that the DAO approves vault transactions.
    {
        let ms = squads_multisig.try_borrow_data()?;
        // Need bytes through the time_lock field before parsing the variable member tail.
        if ms.len() < 78 || ms[..8] != SQUADS_MULTISIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let multisig_config_authority = Pubkey::new_from_array(ms[40..72].try_into().unwrap());
        if multisig_config_authority != *metadao_futarchy.key {
            return Err(ProgramError::InvalidAccountData);
        }
        // Enforce the depositor-protection window on-chain: the bound multisig must impose at least
        // the 1-week timelock the whole DAO->Squads->TWAP->insurance model depends on.
        let time_lock = u32::from_le_bytes(
            ms[SQUADS_TIME_LOCK_OFFSET..SQUADS_TIME_LOCK_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        if time_lock < MIN_TIMELOCK_SECS {
            return Err(ProgramError::InvalidAccountData);
        }
        validate_metadao_squads_control(&ms, metadao_futarchy.key)?;
    }

    let (expected_config, config_bump) = Pubkey::find_program_address(
        &config_seeds(
            market_slab.key,
            squads_multisig.key,
            coin_mint.key,
            percolator_program.key,
        ),
        program_id,
    );
    if *config_account.key != expected_config {
        return Err(ProgramError::InvalidSeeds);
    }
    if config_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let (_twap_authority, authority_bump) =
        Pubkey::find_program_address(&authority_seeds(&expected_config), program_id);

    let bump_arr = [config_bump];
    let seeds: [&[u8]; 6] = [
        CONFIG_SEED,
        market_slab.key.as_ref(),
        squads_multisig.key.as_ref(),
        coin_mint.key.as_ref(),
        percolator_program.key.as_ref(),
        &bump_arr,
    ];
    create_pda_robust(
        payer,
        config_account,
        system_program,
        program_id,
        &seeds,
        CONFIG_SIZE,
    )?;

    let config = Config {
        coin_mint: *coin_mint.key,
        market_slab: *market_slab.key,
        percolator_program: *percolator_program.key,
        squads_multisig: *squads_multisig.key,
        metadao_futarchy: *metadao_futarchy.key,
        surplus_buy_burn_bps: DEFAULT_SURPLUS_BUY_BURN_BPS, // 80% to the auction (burned by default)
        market_0_domain: 0,
        config_bump,
        authority_bump,
        // No pulls until a funded pool handoff imports principal or the legacy
        // unfunded migration explicitly sets a real floor.
        reserved_floor: u128::MAX,
        // 4-way economics default: 80% burn / 0% savings / 0% buyback / 20% insurance growth. The DAO
        // tunes savings + buyback (and their sink accounts) later via timelock'd setters.
        base_unit_savings_bps: 0,
        buyback_bps: 0,
        base_unit_savings_account: Pubkey::default(),
        custody_pool: Pubkey::default(),
        custody_principal: 0,
        custody_mode: CUSTODY_MODE_UNATTESTED,
        rehandoff_pending: false,
        buyback_remainder_bps: 0,
        external_surplus_remainder_bps: 0,
        auction_split_remainder_bps: 0,
        custody_insurance_spent: 0,
    };
    config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    Ok(())
}

// reconfigure accounts: [squads_vault(signer), config(w)]
// data: new_surplus_buy_burn_bps (u16)
//
// Squads -> TWAP control: only the config's Squads multisig default vault PDA may
// reconfigure, and that PDA can only sign as the executor of a multisig
// vault-transaction — which requires a DAO proposal to clear the 1-week timelock.
fn process_reconfigure(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;

    if data.len() != 2 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let new_bps = u16::from_le_bytes(data.try_into().unwrap());
    // 0..=100% — the DAO's burn-percentage authority. 0% burns nothing (all surplus retained for
    // insurance growth); 100% burns the entire surplus. `execute` enforces this share at pull time.
    if new_bps > BPS_DENOMINATOR {
        return Err(ProgramError::InvalidInstructionData);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    // Canonical DAO gate (finding IE): use require_squads_vault so the burn-bps setter cannot
    // diverge from the gate every other setter uses.
    require_squads_vault(squads_vault, &config)?;
    // The auction (surplus_buy_burn_bps) and savings (base_unit_savings_bps) pulls must never collectively
    // exceed 100% of the surplus — set_economics enforces this, but reconfigure sets surplus_buy_burn_bps
    // independently and must hold the SAME invariant (finding KN). Otherwise a valid-looking DAO reconfigure
    // could raise the burn share above 10_000 - savings_bps and make every `execute` underflow-revert when it
    // computes `retained = surplus - burnable - savings` — permanently bricking the surplus auction until a
    // corrective reconfigure. Principal is never at risk either way (execute reverts pre-pull), but this keeps
    // the two setters consistent so the config can't be driven into an un-executable state.
    if (new_bps as u32) + (config.base_unit_savings_bps as u32) > BPS_DENOMINATOR as u32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    // This remainder's denominator is auction+savings bps. A policy change starts a new
    // apportionment interval; resetting less than one atom cannot expose principal and avoids
    // interpreting the old ratio under a different denominator.
    config.auction_split_remainder_bps = 0;
    config.surplus_buy_burn_bps = new_bps;
    config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    Ok(())
}

// set_economics accounts: [squads_vault(signer), config(w), savings_account(ro)]
// data: base_unit_savings_bps (u16) || buyback_bps (u16)
//
// Squads-vault-gated (timelock'd) DAO control of the 4-way surplus split. Sets the savings share (surplus
// withdrawn to the base-unit/collateral savings sink) and the buyback share (of the auction's bought COIN,
// the fraction retained to the book's COIN sink rather than burned), and binds the savings sink account.
// PRINCIPAL-PROTECTION VALIDATION: surplus_buy_burn_bps + base_unit_savings_bps <= 10_000, so the auction
// pull and the savings pull together can never exceed the surplus (insurance_growth = remainder stays >= 0;
// neither tag-57 pull can reach the reserved principal floor); and buyback_bps <= 10_000 (a fraction of the
// bought COIN, applied post-purchase at settle — it never touches insurance/principal, so it is bounded only
// by 100%; the COIN sink itself is the book's coin_sink, configured separately via set_coin_sink). A
// non-default savings account is required once
// the savings share is non-zero, so surplus is never withdrawn to the zero address. The sink must be a
// twap_authority(operator)-owned collateral token account — percolator's tag-57 forces every insurance
// withdrawal to an operator-owned destination — so the savings accrue in a segregated twap-owned reserve
// the DAO governs via Squads; that owner/mint pairing is checked by percolator (and the mint by execute).
fn process_set_economics(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let savings_account = next_account_info(iter)?;

    if data.len() != 4 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let savings_bps = u16::from_le_bytes(data[..2].try_into().unwrap());
    let buyback_bps = u16::from_le_bytes(data[2..4].try_into().unwrap());
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    require_squads_vault(squads_vault, &config)?;
    // The two surplus pulls (auction + savings) must never collectively exceed 100% of the surplus, so the
    // insurance-growth remainder stays >= 0 and neither pull can reach the reserved principal floor.
    if (config.surplus_buy_burn_bps as u32) + (savings_bps as u32) > BPS_DENOMINATOR as u32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    // buyback_bps is the fraction of the AUCTION's bought COIN retained to the COIN sink (vs burned) at
    // settle — a post-purchase split that never touches insurance/principal, so it is bounded only by 100%.
    if buyback_bps > BPS_DENOMINATOR {
        return Err(ProgramError::InvalidInstructionData);
    }
    if savings_bps > 0 && *savings_account.key == Pubkey::default() {
        return Err(ProgramError::InvalidAccountData);
    }
    if savings_bps > 0 {
        if savings_account.owner != &spl_token::ID {
            return Err(ProgramError::IllegalOwner);
        }
        let sink = spl_token::state::Account::unpack(&savings_account.try_borrow_data()?)?;
        let authority_bump = [config.authority_bump];
        let expected_authority = Pubkey::create_program_address(
            &[
                TWAP_AUTHORITY_SEED,
                config_account.key.as_ref(),
                &authority_bump,
            ],
            program_id,
        )
        .map_err(|_| ProgramError::InvalidSeeds)?;
        // SPL owner rotation clears a delegate but preserves a non-native account's explicit
        // close authority. Reject every externally mutable sink before the timelocked config
        // commits it; otherwise the retained close signer can brick all permissionless rounds.
        if sink.state != spl_token::state::AccountState::Initialized
            || sink.owner != expected_authority
            || sink.delegate.is_some()
            || sink.delegated_amount != 0
            || sink.close_authority.is_some()
        {
            return Err(ProgramError::InvalidAccountData);
        }
    }
    // See reconfigure: the combined external carry has the fixed 10_000 denominator and remains
    // valid across policy changes, while this route-level carry uses auction+savings as denominator.
    config.auction_split_remainder_bps = 0;
    config.base_unit_savings_bps = savings_bps;
    config.buyback_bps = buyback_bps;
    config.base_unit_savings_account = *savings_account.key;
    config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    Ok(())
}

// set_reserved_floor accounts: [squads_vault(signer), config(w)]
// data: new_reserved_floor (u128)
//
// Squads -> TWAP control: set or raise the surplus floor. Canonical funded custody
// initializes the floor from subledger accounting during handoff. This setter remains
// for unfunded legacy migration and conservative post-handoff increases.
fn process_set_reserved_floor(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;

    if data.len() != 16 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let new_floor = u128::from_le_bytes(data.try_into().unwrap());
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    // Canonical DAO gate (finding IE): use require_squads_vault so the floor setter — the
    // principal-drain guard — can never diverge from the gate the other setters use.
    require_squads_vault(squads_vault, &config)?;
    // Monotonic after the initial set (finding II): once a REAL floor is set (i.e. it is no longer the
    // u128::MAX "unset" sentinel), it can only ever RISE. This enforces README Safety §5 ("the protected
    // principal only ever grows; principal is never in scope") ON-CHAIN: post-handoff, depositor exits
    // are closed (finding S), so the §3 "exit during the timelock window" backstop no longer protects
    // them — without this, a captured/malicious DAO could lower the floor (even via a timelock'd Squads
    // execute) and drain the now-locked depositor principal as "surplus" through execute -> buy-burn.
    // The single allowed decrease is the initial MAX -> principal set at handoff. To RETURN principal,
    // the DAO re-grants the subledger operator and depositors exit (the documented recovery), never by
    // lowering the floor into principal.
    //
    // CRITICAL: u128::MAX is BOTH the unset sentinel AND a valid maximal value, so a real floor must NEVER be
    // raised back to MAX — doing so would re-arm the sentinel and re-enable an unbounded lower (raise principal
    // -> MAX, which `MAX < principal == false` permits, then MAX -> 0, which the `!= MAX` guard skips): a 2-step
    // bypass of the monotonicity that re-exposes the locked depositor principal as surplus -> execute drains it.
    // A legitimate "pause all pulls" is still expressible as any high REAL value (surplus saturates to 0); the
    // sentinel itself is reserved for the pre-handoff unset state only.
    if config.reserved_floor != u128::MAX
        && (new_floor < config.reserved_floor || new_floor == u128::MAX)
    {
        return Err(ProgramError::InvalidArgument);
    }
    config.reserved_floor = new_floor;
    config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    Ok(())
}

// accept_operator accounts: [squads_vault/current_admin(signer), config(w for current layout),
//   twap_authority(pda), market_slab(w), percolator_program]
//
// Legacy migration path for an unfunded market whose Squads vault is still the raw
// asset_admin. It moves the insurance roles AND asset_admin to the constrained TWAP
// PDA. Canonical funded genesis uses IX_ACCEPT_FROM_SUBLEDGER below, because the
// owner-bound pool already holds asset_admin before the first user deposit.
//
// After this, `execute` (permissionless) is the operator's only insurance path, and it
// is surplus-floor-bounded (finding O fixed): it pulls at most `insurance - reserved_floor`.
// The DAO proposal that performs the handoff should also set the reserved_floor (to the
// reserved depositor principal) via set_reserved_floor and rotate the policy to surplus-mode
// — until reserved_floor is set it is u128::MAX, so no surplus can be pulled at all.
fn process_accept_operator(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    process_accept_custody(
        program_id,
        squads_vault,
        squads_vault,
        config_account,
        twap_authority,
        market_slab,
        percolator_program,
        None,
    )
}

// accept_from_subledger accounts: [squads_vault(signer on first handoff), pool(current_admin signer),
//   config, twap_authority, market_slab(w), percolator_program]
// data: protected_owner_insurance(u128) |
//   aggregate_insurance_spent(u128, present for cross-backed pools), supplied by the fixed CPI.
//
// The call is reachable through subledger::handoff_to_twap only: Squads chooses the
// first handoff, while any caller may resume an established current-layout binding
// after an owner exit. The owner-bound pool supplies the current-admin signature, and
// this program supplies only its canonical config-bound incoming PDA signature.
fn process_accept_from_subledger(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (protected_floor, insurance_spent) = match data.len() {
        16 => (u128::from_le_bytes(data.try_into().unwrap()), None),
        32 => (
            u128::from_le_bytes(data[..16].try_into().unwrap()),
            Some(u128::from_le_bytes(data[16..32].try_into().unwrap())),
        ),
        _ => return Err(ProgramError::InvalidInstructionData),
    };
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let current_admin = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if current_admin.owner != &SUBLEDGER_PROGRAM_ID {
        return Err(ProgramError::IllegalOwner);
    }

    process_accept_custody(
        program_id,
        squads_vault,
        current_admin,
        config_account,
        twap_authority,
        market_slab,
        percolator_program,
        Some((current_admin, protected_floor, insurance_spent)),
    )
}

#[allow(clippy::too_many_arguments)]
fn process_accept_custody<'a>(
    program_id: &Pubkey,
    squads_vault: &AccountInfo<'a>,
    current_admin: &AccountInfo<'a>,
    config_account: &AccountInfo<'a>,
    twap_authority: &AccountInfo<'a>,
    market_slab: &AccountInfo<'a>,
    percolator_program: &AccountInfo<'a>,
    source_pool: Option<(&AccountInfo<'a>, u128, Option<u128>)>,
) -> ProgramResult {
    if !current_admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let established_rehandoff = if let Some((pool, _, _)) = source_pool {
        config_account.data_len() >= PROVENANCE_CONFIG_SIZE
            && config.custody_mode == CUSTODY_MODE_POOL_BOUND
            && config.custody_pool == *pool.key
            && config.rehandoff_pending
    } else {
        false
    };
    if !squads_vault.is_signer && !established_rehandoff {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let mut persist_config = false;
    if let Some((pool, protected_floor, insurance_spent)) = source_pool {
        if !config_account.is_writable || config_account.data_len() < CUSTODY_CONFIG_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if config.custody_pool != Pubkey::default() && config.custody_pool != *pool.key {
            return Err(ProgramError::InvalidAccountData);
        }
        let previous_pool_principal = config.custody_principal as u128;
        let new_pool_principal =
            u64::try_from(protected_floor).map_err(|_| ProgramError::InvalidAccountData)?;
        let is_rehandoff = config.custody_pool == *pool.key;
        config.custody_pool = *pool.key;
        if config_account.data_len() >= PROVENANCE_CONFIG_SIZE && is_rehandoff {
            // The fixed subledger CPI is the only path allowed to lower any part of the floor.
            // Remove exactly the pool principal recorded at the previous handoff, preserving
            // every retained/DAO-raised buffer, then add the pool's current live principal.
            // This makes return -> owner exits -> re-handoff live without giving governance a
            // selectable floor-decrease primitive or exposing any remaining user principal.
            if config.reserved_floor == u128::MAX {
                return Err(ProgramError::InvalidAccountData);
            }
            let retained_floor = config
                .reserved_floor
                .checked_sub(previous_pool_principal)
                .ok_or(ProgramError::InvalidAccountData)?;
            config.reserved_floor = retained_floor
                .checked_add(protected_floor)
                .ok_or(ProgramError::ArithmeticOverflow)?;
        } else if config.reserved_floor == u128::MAX || config.reserved_floor < protected_floor {
            config.reserved_floor = protected_floor;
        }
        if config_account.data_len() >= PROVENANCE_CONFIG_SIZE {
            config.custody_principal = new_pool_principal;
            config.custody_mode = CUSTODY_MODE_POOL_BOUND;
            config.rehandoff_pending = false;
        }
        if let Some(insurance_spent) = insurance_spent {
            if insurance_spent
                != read_asset_insurance_spent_total(
                    &market_slab.try_borrow_data()?,
                    config.market_0_domain as usize,
                )?
            {
                return Err(ProgramError::InvalidAccountData);
            }
            if config_account.data_len() >= CONFIG_SIZE {
                config.custody_insurance_spent = insurance_spent;
            }
        } else if config_account.data_len() >= CONFIG_SIZE {
            config.custody_insurance_spent = 0;
        }
        persist_config = true;
    }
    if *squads_vault.key != squads_default_vault(&config.squads_multisig) {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != config.market_slab
        || *percolator_program.key != config.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // The pool-less compatibility path has no owner-bound recovery destination. It is safe only
    // before insurance is funded; otherwise this rotation would place existing capital under the
    // TWAP PDA with no public path that can return custody to its provider. Funded handoffs must
    // come through the subledger, which binds `custody_pool` and imports its live principal floor.
    if source_pool.is_none() {
        if config.custody_pool != Pubkey::default()
            || read_asset_insurance(
                &market_slab.try_borrow_data()?,
                config.market_0_domain as usize,
            )? != 0
        {
            return Err(ProgramError::InvalidAccountData);
        }
        // Only current-layout configs can persist proof that a pool-less handoff
        // started empty. Historical unmarked configs keep their existing live
        // behavior but cannot later infer that terminal insurance is protocol-owned.
        if config_account.data_len() >= PROVENANCE_CONFIG_SIZE {
            if !config_account.is_writable {
                return Err(ProgramError::InvalidAccountData);
            }
            config.custody_mode = CUSTODY_MODE_POOLLESS_EMPTY;
            persist_config = true;
        }
    }
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }

    // Terminal recovery derives one controller from this config's Squads vault.
    // Custody may move before lifecycle donation (marketauth is still that vault)
    // or after donation to that exact controller, but never across governance seeds.
    let expected_controller = Pubkey::find_program_address(
        &[
            MARKET_CONTROLLER_SEED,
            squads_vault.key.as_ref(),
            market_slab.key.as_ref(),
            percolator_program.key.as_ref(),
        ],
        &MARKET_CONTROLLER_PROGRAM_ID,
    )
    .0;
    let market_authority =
        percolator_accounting::read_market_authority(&market_slab.try_borrow_data()?)
            .map_err(|_| ProgramError::InvalidAccountData)?;
    if market_authority != squads_vault.key.to_bytes()
        && market_authority != expected_controller.to_bytes()
    {
        return Err(ProgramError::InvalidAccountData);
    }

    // Move both insurance roles and finally asset_admin to the constrained PDA.
    // Percolator's insurance authority is also its resolved-mode terminal withdrawal
    // key, so even a nominally top-up-only governance key would be a principal drain
    // after resolution. Fresh donations use IX_DONATE_INSURANCE instead.
    for (kind, incoming) in [
        (ASSET_AUTH_INSURANCE_OPERATOR, twap_authority.key),
        (ASSET_AUTH_INSURANCE, twap_authority.key),
        (ASSET_AUTH_ADMIN, twap_authority.key),
    ] {
        let mut ix_data = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
        ix_data.extend_from_slice(&0u16.to_le_bytes());
        ix_data.push(kind);
        ix_data.extend_from_slice(incoming.as_ref());
        invoke_signed(
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*current_admin.key, true),
                    AccountMeta::new_readonly(*incoming, true),
                    AccountMeta::new(*market_slab.key, false),
                ],
                data: ix_data,
            },
            &[
                current_admin.clone(),
                squads_vault.clone(),
                twap_authority.clone(),
                market_slab.clone(),
                percolator_program.clone(),
            ],
            &[&auth_seeds],
        )?;
    }
    if persist_config {
        config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    }
    Ok(())
}

// return_to_subledger accounts: [squads_vault(signer unless fixed public return), config,
//   twap_authority(current_admin), pool(w), market_slab(w), percolator_program,
//   subledger_program, owner(signer, optional), position(w, optional),
//   owner_destination(w, optional), pool_holding(w, optional),
//   percolator_vault(w, optional), vault_authority(optional),
//   cross_backing_long_ledger?, cross_backing_short_ledger?, token_program(optional)]
// data: owner exit = expected_principal(u64) | expected_start_slot(u64) |
//   expected_action_nonce(u64);
//       no-owner governance/resolved return = empty
//
// This is the only recovery key transition exposed by the TWAP. It cannot select an
// arbitrary replacement: the fixed subledger program re-derives the market-bound pool
// and hardcodes all incoming authorities to that pool PDA. Before terminal resolution,
// a non-governance call must atomically complete the signing owner's full subledger
// withdrawal. A failed exit rolls back the role return, and a successful exit consumes
// the claim that authorized it. Any caller may subsequently re-handoff the established
// pool, so an owner exit cannot permanently interrupt the auction.
fn process_return_to_subledger(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let pool = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let subledger_program = next_account_info(iter)?;
    let (owner_exit, recovery_backing_ledgers) = match iter.as_slice() {
        [] => (None, None),
        [long_backing_ledger, short_backing_ledger] => {
            (None, Some([long_backing_ledger, short_backing_ledger]))
        }
        [owner, position, owner_destination, pool_holding, percolator_vault, vault_authority, token_program] => {
            (
                Some((
                    owner,
                    position,
                    owner_destination,
                    pool_holding,
                    percolator_vault,
                    vault_authority,
                    None,
                    token_program,
                )),
                None,
            )
        }
        [owner, position, owner_destination, pool_holding, percolator_vault, vault_authority, long_backing_ledger, short_backing_ledger, token_program] => {
            let ledgers = Some([long_backing_ledger, short_backing_ledger]);
            (
                Some((
                    owner,
                    position,
                    owner_destination,
                    pool_holding,
                    percolator_vault,
                    vault_authority,
                    ledgers,
                    token_program,
                )),
                ledgers,
            )
        }
        _ => return Err(ProgramError::InvalidInstructionData),
    };
    let owner_exit_requested = owner_exit.is_some();
    let owner_exit_witness = if owner_exit_requested {
        if data.len() != 24 {
            return Err(ProgramError::InvalidInstructionData);
        }
        Some(data)
    } else {
        if !data.is_empty() {
            return Err(ProgramError::InvalidInstructionData);
        }
        None
    };

    if *subledger_program.key != SUBLEDGER_PROGRAM_ID || !subledger_program.executable {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if *squads_vault.key != squads_default_vault(&config.squads_multisig) {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != config.market_slab
        || *percolator_program.key != config.percolator_program
        || config.custody_pool == Pubkey::default()
        || *pool.key != config.custody_pool
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if !pool.is_writable {
        return Err(ProgramError::InvalidAccountData);
    }
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }
    if squads_vault.is_signer {
        if owner_exit.is_some() {
            return Err(ProgramError::InvalidInstructionData);
        }
    } else {
        if !percolator_program.executable || market_slab.owner != percolator_program.key {
            return Err(ProgramError::MissingRequiredSignature);
        }
        let market_data = market_slab.try_borrow_data()?;
        let resolved_empty = percolator_accounting::market_is_resolved_and_empty(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let insurance = if resolved_empty {
            Some(read_asset_insurance(
                &market_data,
                config.market_0_domain as usize,
            )?)
        } else {
            None
        };
        drop(market_data);

        if let Some(insurance) = insurance {
            if owner_exit.is_some() {
                return Err(ProgramError::InvalidInstructionData);
            }
            if insurance != 0 {
                // The subledger owns both current and historical pool layouts. Its
                // read-only attestation prevents a public caller from rotating terminal
                // protocol insurance into a pool after every owner claim has exited.
                invoke(
                    &Instruction {
                        program_id: *subledger_program.key,
                        accounts: vec![AccountMeta::new_readonly(*pool.key, false)],
                        data: vec![SUBLEDGER_IX_ASSERT_PRINCIPAL],
                    },
                    &[pool.clone(), subledger_program.clone()],
                )?;
            }
            // Once asset-local insurance is zero, returning asset_admin and the two
            // insurance roles moves no value. The exact pool is still config-bound and
            // its accept CPI revalidates the canonical Subledger PDA. This lets that
            // pool invoke the fixed asset-0 backing cleanup when its provider is absent.
        } else {
            if config_account.data_len() < PROVENANCE_CONFIG_SIZE
                || config.custody_mode != CUSTODY_MODE_POOL_BOUND
                || config.rehandoff_pending
            {
                return Err(ProgramError::MissingRequiredSignature);
            }
            let (
                owner,
                position,
                owner_destination,
                pool_holding,
                percolator_vault,
                _,
                backing_ledgers,
                token_program,
            ) = owner_exit.ok_or(ProgramError::MissingRequiredSignature)?;
            if !owner.is_signer
                || !position.is_writable
                || !owner_destination.is_writable
                || !pool_holding.is_writable
                || !percolator_vault.is_writable
                || !config_account.is_writable
                || backing_ledgers
                    .is_some_and(|ledgers| ledgers.into_iter().any(|ledger| !ledger.is_writable))
            {
                return Err(ProgramError::MissingRequiredSignature);
            }
            if *token_program.key != spl_token::ID {
                return Err(ProgramError::IncorrectProgramId);
            }
        }
    }

    let pool_meta = if pool.is_writable {
        AccountMeta::new(*pool.key, false)
    } else {
        AccountMeta::new_readonly(*pool.key, false)
    };
    let mut accept_accounts = vec![
        AccountMeta::new_readonly(*twap_authority.key, true),
        pool_meta,
        AccountMeta::new(*market_slab.key, false),
        AccountMeta::new_readonly(*percolator_program.key, false),
        AccountMeta::new_readonly(*config_account.key, false),
    ];
    let mut accept_infos = vec![
        twap_authority.clone(),
        pool.clone(),
        market_slab.clone(),
        percolator_program.clone(),
        config_account.clone(),
    ];
    if let Some(ledgers) = recovery_backing_ledgers {
        for ledger in ledgers {
            accept_accounts.push(AccountMeta::new_readonly(*ledger.key, false));
            accept_infos.push(ledger.clone());
        }
    }
    accept_infos.push(subledger_program.clone());
    invoke_signed(
        &Instruction {
            program_id: *subledger_program.key,
            accounts: accept_accounts,
            data: vec![SUBLEDGER_IX_ACCEPT_OPERATOR],
        },
        &accept_infos,
        &[&auth_seeds],
    )?;

    if let Some((
        owner,
        position,
        owner_destination,
        pool_holding,
        percolator_vault,
        vault_authority,
        backing_ledgers,
        token_program,
    )) = owner_exit
    {
        let mut withdraw_data = vec![SUBLEDGER_IX_INSURANCE_WITHDRAW_FULL];
        withdraw_data.extend_from_slice(
            owner_exit_witness.ok_or(ProgramError::InvalidInstructionData)?,
        );
        let mut withdraw_accounts = vec![
            AccountMeta::new_readonly(*owner.key, true),
            AccountMeta::new(*pool.key, false),
            AccountMeta::new(*position.key, false),
            AccountMeta::new(*owner_destination.key, false),
            AccountMeta::new(*pool_holding.key, false),
            AccountMeta::new(*market_slab.key, false),
            AccountMeta::new(*percolator_vault.key, false),
            AccountMeta::new_readonly(*vault_authority.key, false),
        ];
        let mut withdraw_infos = vec![
            owner.clone(),
            pool.clone(),
            position.clone(),
            owner_destination.clone(),
            pool_holding.clone(),
            market_slab.clone(),
            percolator_vault.clone(),
            vault_authority.clone(),
        ];
        if let Some(ledgers) = backing_ledgers {
            for ledger in ledgers {
                withdraw_accounts.push(AccountMeta::new(*ledger.key, false));
                withdraw_infos.push(ledger.clone());
            }
        }
        withdraw_accounts.extend_from_slice(&[
            AccountMeta::new_readonly(*percolator_program.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ]);
        withdraw_infos.extend_from_slice(&[
            percolator_program.clone(),
            token_program.clone(),
            subledger_program.clone(),
        ]);
        invoke(
            &Instruction {
                program_id: *subledger_program.key,
                accounts: withdraw_accounts,
                data: withdraw_data,
            },
            &withdraw_infos,
        )?;
    }
    if owner_exit_requested {
        config.rehandoff_pending = true;
        config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    }
    Ok(())
}

// return_resolved_protocol_insurance accounts:
// [config, twap_authority, custody_pool_or_system, market(w), twap_transit(w),
//  governance_destination(w), percolator_vault(w), vault_authority,
//  percolator_program, subledger_program, token_program]
//
// Once the market is resolved and the bound pool attests that every
// owner claim is gone, the monotonic retained floor is protocol insurance rather
// than user principal. Anyone may move the exact remaining asset-0 balance through
// a clean TWAP-owned transit into a clean account owned by the bound Squads vault.
// Paying the fixed governance owner immediately prevents this public path from
// creating a second persistent controller balance alongside secondary protocol
// insurance. Requiring an empty TWAP transit prevents an unrelated TWAP balance
// from being swept, while replaceable accounts preserve frozen-ATA liveness.
fn process_return_resolved_protocol_insurance(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let pool = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let twap_transit = next_account_info(iter)?;
    let governance_destination = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let subledger_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market_slab.is_writable
        || !twap_transit.is_writable
        || !governance_destination.is_writable
        || !percolator_vault.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *subledger_program.key != SUBLEDGER_PROGRAM_ID
        || !subledger_program.executable
        || *token_program.key != spl_token::ID
        || !percolator_program.executable
        || market_slab.owner != percolator_program.key
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let pool_bound = config.custody_pool != Pubkey::default();
    if config.market_0_domain != 0
        || *market_slab.key != config.market_slab
        || *percolator_program.key != config.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if pool_bound {
        if pool.owner != &SUBLEDGER_PROGRAM_ID || *pool.key != config.custody_pool {
            return Err(ProgramError::InvalidAccountData);
        }
    } else if config.custody_mode != CUSTODY_MODE_POOLLESS_EMPTY
        || config.custody_principal != 0
        || *pool.key != solana_program::system_program::ID
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }

    if pool_bound {
        // The subledger owns the claim accounting and its historical layouts. A
        // successful read-only CPI is the proof that no owner principal or shares remain.
        invoke(
            &Instruction {
                program_id: *subledger_program.key,
                accounts: vec![AccountMeta::new_readonly(*pool.key, false)],
                data: vec![SUBLEDGER_IX_ASSERT_NO_PRINCIPAL],
            },
            &[pool.clone(), subledger_program.clone()],
        )?;
    }

    let governance = squads_default_vault(&config.squads_multisig);
    let controller = Pubkey::find_program_address(
        &[
            MARKET_CONTROLLER_SEED,
            governance.as_ref(),
            market_slab.key.as_ref(),
            percolator_program.key.as_ref(),
        ],
        &MARKET_CONTROLLER_PROGRAM_ID,
    )
    .0;
    let amount = {
        let market_data = market_slab.try_borrow_data()?;
        if percolator_accounting::read_market_authority(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != controller.to_bytes()
            || !percolator_accounting::market_is_resolved_and_empty(&market_data)
                .map_err(|_| ProgramError::InvalidAccountData)?
            || percolator_accounting::read_asset_insurance_authority(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?
                != twap_authority.key.to_bytes()
            || percolator_accounting::read_asset_insurance_operator(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?
                != twap_authority.key.to_bytes()
            || percolator_accounting::read_asset_admin(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?
                != twap_authority.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let amount = read_asset_insurance(&market_data, 0)?;
        if amount == 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        amount
    };
    let amount_u64 = as_u64(amount)?;

    if twap_transit.owner != &spl_token::ID
        || governance_destination.owner != &spl_token::ID
        || twap_transit.key == governance_destination.key
    {
        return Err(ProgramError::IllegalOwner);
    }
    let twap_state = spl_token::state::Account::unpack(&twap_transit.try_borrow_data()?)?;
    let destination_state =
        spl_token::state::Account::unpack(&governance_destination.try_borrow_data()?)?;
    if twap_state.state != spl_token::state::AccountState::Initialized
        || destination_state.state != spl_token::state::AccountState::Initialized
        || twap_state.owner != *twap_authority.key
        || destination_state.owner != governance
        || twap_state.mint != destination_state.mint
        || twap_state.amount != 0
        || twap_state.delegate.is_some()
        || twap_state.delegated_amount != 0
        || twap_state.close_authority.is_some()
        || destination_state.delegate.is_some()
        || destination_state.delegated_amount != 0
        || destination_state.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let mut ix_data = vec![PERC_IX_WITHDRAW_INSURANCE_ASSET];
    ix_data.extend_from_slice(&0u16.to_le_bytes());
    ix_data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*twap_authority.key, true),
                AccountMeta::new(*market_slab.key, false),
                AccountMeta::new(*twap_transit.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*vault_authority.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: ix_data,
        },
        &[
            twap_authority.clone(),
            market_slab.clone(),
            twap_transit.clone(),
            percolator_vault.clone(),
            vault_authority.clone(),
            token_program.clone(),
            percolator_program.clone(),
        ],
        &[&auth_seeds],
    )?;

    let returned_amount =
        spl_token::state::Account::unpack(&twap_transit.try_borrow_data()?)?.amount;
    if returned_amount != amount_u64 {
        return Err(ProgramError::InvalidAccountData);
    }
    spl_transfer(
        token_program,
        twap_transit,
        governance_destination,
        twap_authority,
        amount_u64,
        Some(&auth_seeds),
    )?;
    invoke_signed(
        &spl_token::instruction::close_account(
            token_program.key,
            twap_transit.key,
            governance_destination.key,
            twap_authority.key,
            &[],
        )?,
        &[
            twap_transit.clone(),
            governance_destination.clone(),
            twap_authority.clone(),
            token_program.clone(),
        ],
        &[&auth_seeds],
    )
}

// return_resolved_asset0_backing accounts:
// [config, twap_authority, governance, controller, market(w), provider_ata(w),
//  controller_transit(w), percolator_vault(w), vault_authority,
//  long_backing_ledger(w), short_backing_ledger(w), percolator_program,
//  market_controller_program, token_program]
//
// A pool-less config has no Subledger PDA to sign the controller's fixed asset-0
// cleanup. This wrapper supplies only the config-bound TWAP signer. The controller
// derives the backing provider and both withdrawal amounts from the pinned slab.
fn process_return_resolved_asset0_backing(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let provider_destination = next_account_info(iter)?;
    let controller_transit = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let long_backing_ledger = next_account_info(iter)?;
    let short_backing_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let controller_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market_slab.is_writable
        || !provider_destination.is_writable
        || !controller_transit.is_writable
        || !percolator_vault.is_writable
        || !long_backing_ledger.is_writable
        || !short_backing_ledger.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *controller_program.key != MARKET_CONTROLLER_PROGRAM_ID
        || !controller_program.executable
        || *token_program.key != spl_token::ID
        || !percolator_program.executable
        || market_slab.owner != percolator_program.key
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config.market_0_domain != 0
        || config.custody_pool != Pubkey::default()
        || *market_slab.key != config.market_slab
        || *percolator_program.key != config.percolator_program
        || *governance.key != squads_default_vault(&config.squads_multisig)
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }
    let expected_controller = Pubkey::find_program_address(
        &[
            MARKET_CONTROLLER_SEED,
            governance.key.as_ref(),
            market_slab.key.as_ref(),
            percolator_program.key.as_ref(),
        ],
        &MARKET_CONTROLLER_PROGRAM_ID,
    )
    .0;
    if *controller.key != expected_controller {
        return Err(ProgramError::InvalidSeeds);
    }

    invoke_signed(
        &Instruction {
            program_id: *controller_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*governance.key, false),
                AccountMeta::new_readonly(*controller.key, false),
                AccountMeta::new_readonly(*twap_authority.key, true),
                AccountMeta::new(*market_slab.key, false),
                AccountMeta::new(*provider_destination.key, false),
                AccountMeta::new(*controller_transit.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*vault_authority.key, false),
                AccountMeta::new(*long_backing_ledger.key, false),
                AccountMeta::new(*short_backing_ledger.key, false),
                AccountMeta::new_readonly(*percolator_program.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: vec![MARKET_CONTROLLER_IX_RETURN_RESOLVED_ASSET0_BACKING],
        },
        &[
            governance.clone(),
            controller.clone(),
            twap_authority.clone(),
            market_slab.clone(),
            provider_destination.clone(),
            controller_transit.clone(),
            percolator_vault.clone(),
            vault_authority.clone(),
            long_backing_ledger.clone(),
            short_backing_ledger.clone(),
            percolator_program.clone(),
            token_program.clone(),
            controller_program.clone(),
        ],
        &[&auth_seeds],
    )
}

// accept_cross_backing_earnings accounts:
// [pool(s), config, governance, market, pool_transit(w),
//  governance_destination(w), percolator_program, token_program]
// data: exact earnings amount (u64), optional retained principal (u64)
//
// Subledger derives the amount from its canonical pool escrow plus both live
// backing-earnings counters and enters with its canonical pool PDA signer. This
// fixed leg can move only that amount into a clean token account owned by the
// config-bound Squads vault. Current Subledger pools also attest any staged
// owner principal that must remain in the source escrow. It exposes no principal
// withdrawal or admin action.
fn process_accept_cross_backing_earnings(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 8 && data.len() != 16 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let amount = u64::from_le_bytes(data[..8].try_into().unwrap());
    let retained_principal = if data.len() == 16 {
        u64::from_le_bytes(data[8..16].try_into().unwrap())
    } else {
        0
    };
    if amount == 0 {
        return Err(ProgramError::InvalidArgument);
    }
    let iter = &mut accounts.iter();
    let pool = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let governance = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let pool_transit = next_account_info(iter)?;
    let governance_destination = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !pool_transit.is_writable
        || !governance_destination.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if !pool.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if pool.owner != &SUBLEDGER_PROGRAM_ID || config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *token_program.key != spl_token::ID
        || !percolator_program.executable
        || market_slab.owner != percolator_program.key
    {
        return Err(ProgramError::IncorrectProgramId);
    }

    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config_account.data_len() < PROVENANCE_CONFIG_SIZE
        || config.market_0_domain != 0
        || config.custody_mode != CUSTODY_MODE_POOL_BOUND
        || config.custody_pool != *pool.key
        || config.market_slab != *market_slab.key
        || config.percolator_program != *percolator_program.key
        || *governance.key != squads_default_vault(&config.squads_multisig)
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if percolator_accounting::read_asset_backing_authority(
        &market_slab.try_borrow_data()?,
        0,
    )
    .map_err(|_| ProgramError::InvalidAccountData)?
        != pool.key.to_bytes()
    {
        return Err(ProgramError::InvalidAccountData);
    }

    if pool_transit.owner != &spl_token::ID
        || governance_destination.owner != &spl_token::ID
        || pool_transit.key == governance_destination.key
    {
        return Err(ProgramError::IllegalOwner);
    }
    let source = spl_token::state::Account::unpack(&pool_transit.try_borrow_data()?)?;
    let destination =
        spl_token::state::Account::unpack(&governance_destination.try_borrow_data()?)?;
    let source_before = amount
        .checked_add(retained_principal)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if source.state != spl_token::state::AccountState::Initialized
        || destination.state != spl_token::state::AccountState::Initialized
        || source.owner != *pool.key
        || destination.owner != *governance.key
        || source.mint != destination.mint
        || source.amount != source_before
        || source.delegate.is_some()
        || source.delegated_amount != 0
        || source.close_authority.is_some()
        || source.is_native.is_some()
        || destination.delegate.is_some()
        || destination.delegated_amount != 0
        || destination.close_authority.is_some()
        || destination.is_native.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let destination_after = destination
        .amount
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    spl_transfer(
        token_program,
        pool_transit,
        governance_destination,
        pool,
        amount,
        None,
    )?;
    if spl_token::state::Account::unpack(&pool_transit.try_borrow_data()?)?.amount
        != retained_principal
        || spl_token::state::Account::unpack(&governance_destination.try_borrow_data()?)?.amount
            != destination_after
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

// donate_insurance accounts: [donor(s), config, twap_authority,
//   donor_source(w), twap_holding(w), market_slab(w), percolator_vault(w),
//   percolator_program, token_program]
// data: amount (u64) || expected_market_id (u64)
//
// This replaces the unsafe pattern of leaving insurance_authority on governance.
// It can only move donor-owned collateral inward: donor -> a TWAP-owned holding ->
// the config-bound Percolator market. Any downstream failure rolls back both CPIs.
fn process_donate_insurance(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 16 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let amount = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let expected_market_id = u64::from_le_bytes(data[8..16].try_into().unwrap());
    if amount == 0 || expected_market_id == 0 {
        return Err(ProgramError::InvalidArgument);
    }
    let iter = &mut accounts.iter();
    let donor = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let donor_source = next_account_info(iter)?;
    let twap_holding = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !donor.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if *market_slab.key != config.market_slab
        || market_slab.owner != percolator_program.key
        || *percolator_program.key != config.percolator_program
        || !percolator_program.executable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let current_market_id =
        percolator_accounting::read_asset_market_id(&market_slab.try_borrow_data()?, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
    if current_market_id != expected_market_id {
        return Err(ProgramError::InvalidAccountData);
    }
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }

    let source = spl_token::state::Account::unpack(&donor_source.try_borrow_data()?)?;
    let holding = spl_token::state::Account::unpack(&twap_holding.try_borrow_data()?)?;
    if source.owner != *donor.key
        || holding.owner != *twap_authority.key
        || source.mint != holding.mint
        || source.amount < amount
    {
        return Err(ProgramError::InvalidAccountData);
    }

    invoke(
        &spl_token::instruction::transfer(
            token_program.key,
            donor_source.key,
            twap_holding.key,
            donor.key,
            &[],
            amount,
        )?,
        &[
            donor_source.clone(),
            twap_holding.clone(),
            donor.clone(),
            token_program.clone(),
        ],
    )?;

    let mut ix_data = vec![PERC_IX_TOP_UP_INSURANCE];
    ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*twap_authority.key, true),
                AccountMeta::new(*market_slab.key, false),
                AccountMeta::new(*twap_holding.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: ix_data,
        },
        &[
            twap_authority.clone(),
            market_slab.clone(),
            twap_holding.clone(),
            percolator_vault.clone(),
            token_program.clone(),
            percolator_program.clone(),
        ],
        &[&auth_seeds],
    )
}

// set_market_fees accounts:
// [squads_vault(signer), config, twap_authority, market_slab(w), percolator_program]
// data: trade_fee(u64) || long_fee(u16) || long_insurance_share(u16) ||
//       short_fee(u16) || short_insurance_share(u16)
//
// Percolator gates these three policy updates on insurance_authority, which is the
// constrained TWAP PDA after handoff. The pinned program globally rejects atomic
// batches while any backing fee is active, so only zero backing updates are exposed
// until that accounting is batch-safe. Zero updates preserve predecessor recovery.
fn process_set_market_fees(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 16 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let trade_fee = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let long_fee = u16::from_le_bytes(data[8..10].try_into().unwrap());
    let long_insurance = u16::from_le_bytes(data[10..12].try_into().unwrap());
    let short_fee = u16::from_le_bytes(data[12..14].try_into().unwrap());
    let short_insurance = u16::from_le_bytes(data[14..16].try_into().unwrap());
    if long_fee != 0 || long_insurance != 0 || short_fee != 0 || short_insurance != 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if !squads_vault.is_signer || iter.next().is_some() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if *squads_vault.key != squads_default_vault(&config.squads_multisig) {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != config.market_slab
        || market_slab.owner != percolator_program.key
        || *percolator_program.key != config.percolator_program
        || !percolator_program.executable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }
    let current_trade_fee = percolator_accounting::read_trade_fee_base_bps(
        &market_slab.try_borrow_data()?,
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;
    // Trades have no user-supplied maximum for the configured fee. A monotonic
    // decrease is the only local rule that cannot reprice an already-signed trade.
    if trade_fee > current_trade_fee {
        return Err(ProgramError::InvalidInstructionData);
    }

    let mut trade_data = vec![PERC_IX_UPDATE_TRADE_FEE_POLICY];
    trade_data.extend_from_slice(&trade_fee.to_le_bytes());
    let mut long_data = vec![PERC_IX_UPDATE_BACKING_FEE_POLICY];
    long_data.extend_from_slice(&0u16.to_le_bytes());
    long_data.extend_from_slice(&long_fee.to_le_bytes());
    long_data.extend_from_slice(&long_insurance.to_le_bytes());
    let mut short_data = vec![PERC_IX_UPDATE_BACKING_FEE_POLICY];
    short_data.extend_from_slice(&1u16.to_le_bytes());
    short_data.extend_from_slice(&short_fee.to_le_bytes());
    short_data.extend_from_slice(&short_insurance.to_le_bytes());
    for ix_data in [trade_data, long_data, short_data] {
        invoke_signed(
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*twap_authority.key, true),
                    AccountMeta::new(*market_slab.key, false),
                ],
                data: ix_data,
            },
            &[
                twap_authority.clone(),
                market_slab.clone(),
                percolator_program.clone(),
            ],
            &[&auth_seeds],
        )?;
    }
    Ok(())
}

// restart_asset0 accounts:
// [squads_vault(signer), config, twap_authority, market_slab(w), percolator_program,
//  custody_pool(w), subledger_program, pool_holding(w), percolator_vault(w),
//  vault_authority, long_backing_ledger(w), short_backing_ledger(w), token_program]
// for a cross-backed pool. Ordinary pool-bound configs stop after subledger_program;
// pool-less compatibility configs retain the original five-account shape.
// data: now_slot(u64) || initial_price(u64) || expected_market_id(u64)
//
// Once custody is handed off, Percolator recognizes only the TWAP PDA as asset_admin.
// This fixed wrapper keeps restart reachable without exposing a generic admin proxy. Cross-backed
// pools supply only their canonical holding, Percolator vault, and canonical ledgers so Subledger
// can stage owner backing without an amount or caller-selected destination. Percolator enforces
// Recovery, empty positions/backing, the real Clock, and preservation of the existing insurance
// and authority fields.
fn process_restart_asset0(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 24 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let now_slot = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let initial_price = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let expected_market_id = u64::from_le_bytes(data[16..24].try_into().unwrap());
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let pool_checkpoint_accounts = if config.custody_pool != Pubkey::default() {
        if (config_account.data_len() >= PROVENANCE_CONFIG_SIZE
            && config.custody_mode != CUSTODY_MODE_POOL_BOUND)
            || (config_account.data_len() >= PROVENANCE_CONFIG_SIZE
                && !config_account.is_writable)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let custody_pool = next_account_info(iter)?;
        let subledger_program = next_account_info(iter)?;
        if custody_pool.owner != &SUBLEDGER_PROGRAM_ID
            || *subledger_program.key != SUBLEDGER_PROGRAM_ID
            || !subledger_program.executable
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let cross_backing = custody_pool
            .try_borrow_data()?
            .get(SUBLEDGER_POOL_FLAGS_OFF)
            .is_some_and(|flags| flags & SUBLEDGER_POOL_FLAG_CROSS_BACKING != 0);
        let backing_accounts = if cross_backing {
            Some((
                next_account_info(iter)?,
                next_account_info(iter)?,
                next_account_info(iter)?,
                [next_account_info(iter)?, next_account_info(iter)?],
                next_account_info(iter)?,
            ))
        } else {
            None
        };
        Some((custody_pool, subledger_program, backing_accounts))
    } else {
        if config.custody_mode == CUSTODY_MODE_POOL_BOUND {
            return Err(ProgramError::InvalidAccountData);
        }
        None
    };
    if iter.next().is_some() {
        return Err(ProgramError::InvalidAccountData);
    }
    require_squads_vault(squads_vault, &config)?;
    if *market_slab.key != config.market_slab
        || market_slab.owner != percolator_program.key
        || *percolator_program.key != config.percolator_program
        || !percolator_program.executable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let current_market_id =
        percolator_accounting::read_asset_market_id(&market_slab.try_borrow_data()?, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
    if expected_market_id == 0 || current_market_id != expected_market_id {
        return Err(ProgramError::InvalidAccountData);
    }
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }

    let pool_bound = pool_checkpoint_accounts.is_some();
    let mut checkpointed_pool_principal = None;
    if let Some((custody_pool, subledger_program, backing_accounts)) = pool_checkpoint_accounts {
        if *custody_pool.key != config.custody_pool
            || !custody_pool.is_writable
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut checkpoint_metas = vec![
            AccountMeta::new_readonly(*twap_authority.key, true),
            AccountMeta::new_readonly(*config_account.key, false),
            AccountMeta::new(*custody_pool.key, false),
            AccountMeta::new(*market_slab.key, false),
            AccountMeta::new_readonly(*percolator_program.key, false),
        ];
        let mut checkpoint_infos = vec![
            twap_authority.clone(),
            config_account.clone(),
            custody_pool.clone(),
            market_slab.clone(),
            percolator_program.clone(),
        ];
        if let Some((
            pool_holding,
            percolator_vault,
            vault_authority,
            backing_ledgers,
            token_program,
        )) = backing_accounts
        {
            if !pool_holding.is_writable
                || !percolator_vault.is_writable
                || !backing_ledgers[0].is_writable
                || !backing_ledgers[1].is_writable
            {
                return Err(ProgramError::InvalidAccountData);
            }
            checkpoint_metas.extend_from_slice(&[
                AccountMeta::new(*pool_holding.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*vault_authority.key, false),
                AccountMeta::new(*backing_ledgers[0].key, false),
                AccountMeta::new(*backing_ledgers[1].key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ]);
            checkpoint_infos.extend_from_slice(&[
                pool_holding.clone(),
                percolator_vault.clone(),
                vault_authority.clone(),
                backing_ledgers[0].clone(),
                backing_ledgers[1].clone(),
                token_program.clone(),
            ]);
        }
        checkpoint_infos.push(subledger_program.clone());
        invoke_signed(
            &Instruction {
                program_id: *subledger_program.key,
                accounts: checkpoint_metas,
                data: vec![SUBLEDGER_IX_PREPARE_ASSET0_RESTART],
            },
            &checkpoint_infos,
            &[&auth_seeds],
        )?;
        let (return_program, return_data) =
            get_return_data().ok_or(ProgramError::InvalidAccountData)?;
        if return_program != SUBLEDGER_PROGRAM_ID
            || return_data.len() != 24
            || return_data[..8] != RESTART_CHECKPOINT_RETURN_DISC
        {
            return Err(ProgramError::InvalidAccountData);
        }
        checkpointed_pool_principal =
            Some(u128::from_le_bytes(return_data[8..24].try_into().unwrap()));
    }

    let mut ix_data = vec![PERC_IX_RESTART_ASSET_ORACLE];
    ix_data.extend_from_slice(&0u16.to_le_bytes());
    ix_data.extend_from_slice(&now_slot.to_le_bytes());
    ix_data.extend_from_slice(&initial_price.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*twap_authority.key, true),
                AccountMeta::new(*market_slab.key, false),
            ],
            data: ix_data,
        },
        &[
            twap_authority.clone(),
            market_slab.clone(),
            percolator_program.clone(),
        ],
        &[&auth_seeds],
    )?;
    if pool_bound && config_account.data_len() >= PROVENANCE_CONFIG_SIZE {
        let checkpointed_pool_principal =
            checkpointed_pool_principal.ok_or(ProgramError::InvalidAccountData)?;
        let old_pool_principal = u128::from(config.custody_principal);
        let new_pool_principal = u64::try_from(checkpointed_pool_principal)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        if checkpointed_pool_principal > old_pool_principal
            || config.reserved_floor < old_pool_principal
        {
            return Err(ProgramError::InvalidAccountData);
        }
        config.reserved_floor = config
            .reserved_floor
            .checked_sub(old_pool_principal)
            .and_then(|retained| retained.checked_add(checkpointed_pool_principal))
            .ok_or(ProgramError::ArithmeticOverflow)?;
        config.custody_principal = new_pool_principal;
        if config_account.data_len() >= CONFIG_SIZE {
            config.custody_insurance_spent = 0;
        }
        config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Buy/burn uniform-price (Dutch) auction
// ---------------------------------------------------------------------------
//
// A single AuctionBook per config runs time-boxed rounds. During a round anyone may place a bid
// (uncensorable) by escrowing COIN; a placed bid CANNOT be cancelled (anti-spoofing) and only
// leaves the book early by being evicted by a STRICTLY better bid (which refunds it). Once the
// round's slots expire, anyone may `execute`: it pulls the burn-share of the current percolator
// surplus as the auction budget, ratchets the retained share into the principal counter, clears
// the WHOLE book at a single marginal uniform (Dutch) price P* — bids ranked by COIN-per-USD,
// filled best-first until the budget is spent, every filled bid transacting at the marginal rate —
// and BURNS or SENDS the bought COIN (futarchy-configurable). Winners' USD is parked for a
// permissionless `claim`. A DAO-set reserve rate caps the price the protocol will pay.

// AuctionBook header byte offsets (account = book PDA ["twap_book", config]).
const BK_CONFIG: usize = 8;
const BK_COIN_MINT: usize = 40;
const BK_COLLATERAL_MINT: usize = 72;
const BK_COIN_ESCROW: usize = 104;
const BK_SETTLEMENT_USD: usize = 136;
const BK_COIN_SINK: usize = 168; // destination for bought COIN when sink mode == SINK_SEND
const BK_RESERVE_NUM: usize = 200;
const BK_RESERVE_DEN: usize = 216;
const BK_ROUND_LENGTH: usize = 232; // u64: slots a round stays open before execute is allowed
const BK_ROUND_END: usize = 240; // u64: slot at/after which the current round may be executed
const BK_STATE: usize = 248;
const BK_SINK_MODE: usize = 249;
const BK_BOOK_BUMP: usize = 250;
const BK_ESCROW_BUMP: usize = 251;
const BK_HOLDING: usize = 252; // the canonical twap_authority-owned USD budget account
const BK_BID_FEE: usize = 284; // u64: flat COIN fee burned per place_bid (anti-spam, DAO-set)
const BK_SINK_CUTOFF_SLOT: usize = 292; // u64: last slot at which bought COIN may reach coin_sink
const BOOK_HEADER: usize = 300;

// Per-bid slot field offsets, relative to the slot start.
const SL_OCCUPIED: usize = 0;
const SL_SETTLED: usize = 1;
const SL_BIDDER: usize = 2;
const SL_USD_DEST: usize = 34; // collateral token acct that receives the bidder's won USD
const SL_COIN_ATA: usize = 66; // canonical COIN recovery hint retained in the stable slot ABI
const SL_COIN: usize = 98; // coin_atoms escrowed
const SL_USDC: usize = 114; // usdc_atoms wanted (the limit: rate = coin_atoms / usdc_atoms)
const SL_USD_OWED: usize = 130; // set at execute: USD this bid won
const SL_COIN_REFUND: usize = 146; // set at execute: COIN to return (unsold + over-escrow)
const SL_PLACE_SLOT: usize = 162; // u64: slot the bid was placed (cancel after 2*round_length)
const SL_PLACE_ROUND_END: usize = 170; // u64: book.round_end at placement. Bound into the exact cancel
                                       // commitment but NOT a cooldown gate (issue #28: a no-op roll moved
                                       // round_end; cancellation now gates on the aged window alone).
const SLOT_SIZE: usize = 178;
const BOOK_SIZE: usize = BOOK_HEADER + MAX_BIDS * SLOT_SIZE;

#[derive(Clone, Copy)]
struct BookLayout {
    header: usize,
    slot_size: usize,
}

impl BookLayout {
    const fn size(self) -> usize {
        self.header + MAX_BIDS * self.slot_size
    }

    const fn slot_off(self, i: usize) -> usize {
        self.header + i * self.slot_size
    }

    const fn has_cancel_clock(self) -> bool {
        self.slot_size >= SLOT_SIZE
    }
}

const CURRENT_BOOK_LAYOUT: BookLayout = BookLayout {
    header: BOOK_HEADER,
    slot_size: SLOT_SIZE,
};
const LEGACY_PRE_CUTOFF_BOOK_LAYOUT: BookLayout = BookLayout {
    header: BK_SINK_CUTOFF_SLOT,
    slot_size: SLOT_SIZE,
};
const LEGACY_BID_FEE_BOOK_LAYOUT: BookLayout = BookLayout {
    header: BK_SINK_CUTOFF_SLOT,
    slot_size: SL_PLACE_SLOT,
};
const LEGACY_HOLDING_BOOK_LAYOUT: BookLayout = BookLayout {
    header: BK_BID_FEE,
    slot_size: SL_PLACE_SLOT,
};
const INITIAL_BOOK_LAYOUT: BookLayout = BookLayout {
    header: BK_HOLDING,
    slot_size: SL_PLACE_SLOT,
};

fn book_rd_u128(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}
fn book_wr_u128(d: &mut [u8], o: usize, v: u128) {
    d[o..o + 16].copy_from_slice(&v.to_le_bytes());
}
fn book_rd_u64(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn book_rd_key(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}
fn slot_off(i: usize) -> usize {
    BOOK_HEADER + i * SLOT_SIZE
}

struct BookHeader {
    config: Pubkey,
    coin_mint: Pubkey,
    collateral_mint: Pubkey,
    coin_escrow: Pubkey,
    settlement_usd: Pubkey,
    coin_sink: Pubkey,
    holding: Pubkey,
    reserve_num: u128,
    reserve_den: u128,
    round_length: u64,
    round_end: u64,
    bid_fee: u64,
    sink_cutoff_slot: u64,
    state: u8,
    sink_mode: u8,
    #[allow(dead_code)]
    book_bump: u8,
    escrow_bump: u8,
}

fn decode_book_header(d: &[u8], layout: BookLayout) -> Result<BookHeader, ProgramError> {
    if d.len() < layout.size() || d[..8] != BOOK_DISC {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(BookHeader {
        config: book_rd_key(d, BK_CONFIG),
        coin_mint: book_rd_key(d, BK_COIN_MINT),
        collateral_mint: book_rd_key(d, BK_COLLATERAL_MINT),
        coin_escrow: book_rd_key(d, BK_COIN_ESCROW),
        settlement_usd: book_rd_key(d, BK_SETTLEMENT_USD),
        coin_sink: book_rd_key(d, BK_COIN_SINK),
        holding: if layout.header >= BK_BID_FEE {
            book_rd_key(d, BK_HOLDING)
        } else {
            Pubkey::default()
        },
        reserve_num: book_rd_u128(d, BK_RESERVE_NUM),
        reserve_den: book_rd_u128(d, BK_RESERVE_DEN),
        round_length: book_rd_u64(d, BK_ROUND_LENGTH),
        round_end: book_rd_u64(d, BK_ROUND_END),
        bid_fee: if layout.header >= BK_SINK_CUTOFF_SLOT {
            book_rd_u64(d, BK_BID_FEE)
        } else {
            0
        },
        sink_cutoff_slot: if layout.header >= BOOK_HEADER {
            book_rd_u64(d, BK_SINK_CUTOFF_SLOT)
        } else {
            u64::MAX
        },
        state: d[BK_STATE],
        sink_mode: d[BK_SINK_MODE],
        book_bump: d[BK_BOOK_BUMP],
        escrow_bump: d[BK_ESCROW_BUMP],
    })
}

fn load_book_header(d: &[u8]) -> Result<BookHeader, ProgramError> {
    decode_book_header(d, CURRENT_BOOK_LAYOUT)
}

// Legacy books are exit-only. Keeping this decoder private to claim/cancel prevents the retired
// perpetual reward-sink semantics from accepting new bids, executing, or receiving DAO updates.
fn load_exit_book_header(d: &[u8]) -> Result<(BookHeader, BookLayout), ProgramError> {
    let layout = if d.len() >= BOOK_SIZE {
        CURRENT_BOOK_LAYOUT
    } else if d.len() == LEGACY_PRE_CUTOFF_BOOK_LAYOUT.size() {
        LEGACY_PRE_CUTOFF_BOOK_LAYOUT
    } else if d.len() == LEGACY_BID_FEE_BOOK_LAYOUT.size() {
        LEGACY_BID_FEE_BOOK_LAYOUT
    } else if d.len() == LEGACY_HOLDING_BOOK_LAYOUT.size() {
        LEGACY_HOLDING_BOOK_LAYOUT
    } else if d.len() == INITIAL_BOOK_LAYOUT.size() {
        INITIAL_BOOK_LAYOUT
    } else {
        return Err(ProgramError::InvalidAccountData);
    };
    Ok((decode_book_header(d, layout)?, layout))
}

// CONSTANT-TIME comparison of two bid rates coin_a/usdc_a vs coin_b/usdc_b. Both legs are token
// amounts bounded to u64 at place_bid, so the cross-products fit in u128 exactly (u64*u64 < 2^128) —
// no continued-fraction loop. This is what bid-vs-bid ranking uses, so a hostile full book of
// close, long-continued-fraction rates can NOT make the O(N^2) sort blow the compute budget (the
// finding-AC DOS). The bid-vs-reserve path below uses a fixed-work wide cross-product because the
// DAO-set reserve may be a large u128.
fn cmp_bid(coin_a: u128, usdc_a: u128, coin_b: u128, usdc_b: u128) -> core::cmp::Ordering {
    (coin_a * usdc_b).cmp(&(coin_b * usdc_a))
}

// Exact u128*u128 as high/low u128 limbs. Four fixed-width u64 products avoid both overflow and
// attacker-controlled Euclidean loops in the reserve comparator.
fn wide_product(a: u128, b: u128) -> (u128, u128) {
    const MASK: u128 = u64::MAX as u128;
    let a0 = a & MASK;
    let a1 = a >> 64;
    let b0 = b & MASK;
    let b1 = b >> 64;

    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    let middle = (p00 >> 64) + (p01 & MASK) + (p10 & MASK);
    let low = ((middle & MASK) << 64) | (p00 & MASK);
    let high = p11 + (p01 >> 64) + (p10 >> 64) + (middle >> 64);
    (high, low)
}

// Compare a_num/a_den vs b_num/b_den exactly by comparing 256-bit cross-products. All
// denominators are validated as nonzero at their write paths. Unlike continued fractions, this
// takes constant work for every public bid, including adversarial Fibonacci ratios.
fn cmp_rate(an: u128, ad: u128, bn: u128, bd: u128) -> core::cmp::Ordering {
    wide_product(an, bd).cmp(&wide_product(bn, ad))
}

// Bid legs are constrained to positive u64 token amounts. Keeping Stein's algorithm in that native
// width avoids both attacker-priced integer division and costly emulated u128 shift/subtract work.
fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    let common_shift = (a | b).trailing_zeros();
    a >>= a.trailing_zeros();
    loop {
        b >>= b.trailing_zeros();
        if a > b {
            core::mem::swap(&mut a, &mut b);
        }
        b -= a;
        if b == 0 {
            return a << common_shift;
        }
    }
}

fn mul_div_floor(a: u128, b: u128, d: u128) -> Result<u128, ProgramError> {
    a.checked_mul(b)
        .ok_or(ProgramError::ArithmeticOverflow)?
        .checked_div(d)
        .ok_or(ProgramError::ArithmeticOverflow)
}

fn executable_integer_pair(
    nominal_usd: u128,
    marginal_coin: u128,
    marginal_usd: u128,
    bid_coin: u128,
    bid_usd: u128,
) -> Result<Option<(u128, u128)>, ProgramError> {
    if nominal_usd == 0 {
        return Ok(None);
    }
    let coin = mul_div_floor(nominal_usd, marginal_coin, marginal_usd)?;
    if coin == 0 || coin > bid_coin {
        return Ok(None);
    }
    let usd = mul_div_floor(coin, marginal_usd, marginal_coin)?;
    // All legs originate from u64 token amounts, so cmp_bid's u128 cross-product is exact. The
    // reconstructed USD is floored, hence coin/usd >= marginal_coin/marginal_usd; execute has
    // already filtered the marginal bid against the reserve, so a second wide reserve comparison
    // would be redundant attacker-controlled compute.
    if usd == 0
        || usd > nominal_usd
        || cmp_bid(coin, usd, bid_coin, bid_usd) == core::cmp::Ordering::Greater
    {
        return Ok(None);
    }
    Ok(Some((coin, usd)))
}

struct IntervalFraction {
    num: u128,
    den: u128,
    left_parent: Option<(u128, u128)>,
    right_parent: Option<(u128, u128)>,
}

// Find the lowest-denominator fraction in the closed positive interval, together with its
// Stern-Brocot parents. Continued-fraction inversion is Euclidean: u64 inputs terminate in at most
// 128 iterations, independent of the auction budget. The parents let the caller spend additional
// denominator capacity without scanning attacker-selected token amounts one atom at a time.
fn simplest_interval_fraction(
    lower_num: u64,
    lower_den: u64,
    upper_num: u64,
    upper_den: u64,
) -> Result<IntervalFraction, ProgramError> {
    if lower_num == 0
        || lower_den == 0
        || upper_num == 0
        || upper_den == 0
        || (lower_num as u128) * upper_den as u128
            > (upper_num as u128) * lower_den as u128
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let mut ln = lower_num;
    let mut ld = lower_den;
    let mut un = upper_num;
    let mut ud = upper_den;
    let mut terms = [0u64; 128];
    let mut terms_len = 0usize;
    let terminal = loop {
        let lower_term = ln / ld;
        let lower_rem = ln - lower_term * ld;
        if lower_rem == 0 {
            break lower_term;
        }
        // The transformed endpoints remain ordered, so the upper quotient cannot be smaller.
        // Compare it to lower_term + 1 with a wide product instead of paying a second public
        // integer division on every continued-fraction step.
        if un as u128
            >= (lower_term as u128 + 1)
                .checked_mul(ud as u128)
                .ok_or(ProgramError::ArithmeticOverflow)?
        {
            break lower_term
                .checked_add(1)
                .ok_or(ProgramError::ArithmeticOverflow)?;
        }
        if terms_len == terms.len() {
            return Err(ProgramError::ArithmeticOverflow);
        }
        let upper_rem = un - lower_term * ud;
        if upper_rem == 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        terms[terms_len] = lower_term;
        terms_len += 1;
        (ln, ld, un, ud) = (ud, upper_rem, ld, lower_rem);
    };

    // Standard convergent recurrence. The penultimate convergent is one Stern-Brocot parent; the
    // component-wise difference from the result is the other because adjacent determinants are 1.
    let mut prev2_num = 0u128;
    let mut prev_num = 1u128;
    let mut prev2_den = 1u128;
    let mut prev_den = 0u128;
    for i in 0..=terms_len {
        let term = if i == terms_len {
            terminal
        } else {
            terms[i]
        } as u128;
        let next_num = term
            .checked_mul(prev_num)
            .and_then(|value| value.checked_add(prev2_num))
            .ok_or(ProgramError::ArithmeticOverflow)?;
        let next_den = term
            .checked_mul(prev_den)
            .and_then(|value| value.checked_add(prev2_den))
            .ok_or(ProgramError::ArithmeticOverflow)?;
        (prev2_num, prev_num) = (prev_num, next_num);
        (prev2_den, prev_den) = (prev_den, next_den);
    }
    if prev_num == 0 || prev_den == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let other_num = prev_num
        .checked_sub(prev2_num)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let other_den = prev_den
        .checked_sub(prev2_den)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    let previous_is_left = prev2_den != 0
        && prev2_num
            .checked_mul(prev_den)
            .ok_or(ProgramError::ArithmeticOverflow)?
            < prev_num
                .checked_mul(prev2_den)
                .ok_or(ProgramError::ArithmeticOverflow)?;
    let previous = (prev2_num, prev2_den);
    let other = (other_num, other_den);
    let (left_parent, right_parent) = if previous_is_left {
        (previous, other)
    } else {
        (other, previous)
    };

    Ok(IntervalFraction {
        num: prev_num,
        den: prev_den,
        left_parent: (left_parent.1 != 0).then_some(left_parent),
        right_parent: (right_parent.1 != 0).then_some(right_parent),
    })
}

fn consider_larger_pair(
    best: &mut Option<(u128, u128)>,
    pair: Option<(u128, u128)>,
) {
    if let Some(candidate) = pair {
        if best.map_or(true, |current| {
            candidate.1 > current.1 || (candidate.1 == current.1 && candidate.0 > current.0)
        }) {
            *best = Some(candidate);
        }
    }
}

// Maximum practical integer pair for one bid under an already-selected marginal price and
// an actual remaining USD budget. The first candidate is the largest COIN amount whose floored
// marginal-price USD leg fits. If flooring crosses the bidder's limit, fall back to exact reduced
// lots at both endpoints and bounded interior lattice candidates. Every candidate is bidder-safe
// because it stays between the selected marginal and bid rates. All public bid legs are u64-bounded,
// so the products fit in u128, and the caller supplies GCDs for the endpoint ratios.
const MAX_EXCESS_RESIDUES: u128 = 8;

fn max_executable_integer_pair(
    remaining_usd: u128,
    marginal_coin: u128,
    marginal_usd: u128,
    bid_coin: u128,
    bid_usd: u128,
    bid_gcd: u64,
    marginal_gcd: u64,
) -> Result<Option<(u128, u128)>, ProgramError> {
    let max_usd = core::cmp::min(remaining_usd, bid_usd);
    if max_usd == 0 || marginal_coin == 0 || marginal_usd == 0 {
        return Ok(None);
    }

    // floor(coin * marginal_usd / marginal_coin) <= max_usd iff
    // coin * marginal_usd < (max_usd + 1) * marginal_coin.
    let max_coin_for_budget = max_usd
        .checked_add(1)
        .and_then(|v| v.checked_mul(marginal_coin))
        .and_then(|v| v.checked_sub(1))
        .ok_or(ProgramError::ArithmeticOverflow)?
        / marginal_usd;
    let candidate_coin = core::cmp::min(bid_coin, max_coin_for_budget);

    let try_coin = |coin: u128| -> Result<Option<(u128, u128)>, ProgramError> {
        if coin == 0 {
            return Ok(None);
        }
        let usd = mul_div_floor(coin, marginal_usd, marginal_coin)?;
        if usd == 0
            || usd > max_usd
            || cmp_bid(coin, usd, bid_coin, bid_usd) == core::cmp::Ordering::Greater
        {
            return Ok(None);
        }
        Ok(Some((coin, usd)))
    };

    // Preserve the common constant-work path: candidate_coin is already the largest numerator
    // whose marginal-price floor fits the budget, so a bidder-safe result cannot be improved.
    if let Some(pair) = try_coin(candidate_coin)? {
        return Ok(Some(pair));
    }

    // Project through denominators produced by flooring at the marginal rate. At a fixed USD
    // denominator, cap the numerator by both the bidder's limit and the largest COIN amount whose
    // marginal-price floor does not exceed that denominator. If this maximum misses the marginal
    // floor, no smaller numerator can work there; floor(coin*marginal_usd/marginal_coin) is then the
    // largest denominator that numerator can support. Each step therefore skips only impossible
    // denominators, while the caller's hard cap bounds the number of steps.
    let projected_coin_for_usd = |usd: u128| -> Result<u128, ProgramError> {
        let bidder_cap = mul_div_floor(bid_coin, usd, bid_usd)?;
        let marginal_cap = usd
            .checked_add(1)
            .and_then(|value| value.checked_mul(marginal_coin))
            .and_then(|value| value.checked_sub(1))
            .ok_or(ProgramError::ArithmeticOverflow)?
            / marginal_usd;
        Ok(core::cmp::min(bidder_cap, marginal_cap))
    };
    if bid_gcd == 0 || marginal_gcd == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    // The largest rounded candidate can cross the bidder's limit even though a smaller exact
    // marginal-price lot fits. That lot is always safe for a bid ranked at or above the marginal;
    // cap its scale by both the available USD and the bid's deposited COIN.
    let marginal_coin_lot = marginal_coin / marginal_gcd as u128;
    let marginal_usd_lot = marginal_usd / marginal_gcd as u128;
    let marginal_lots = core::cmp::min(
        max_usd / marginal_usd_lot,
        bid_coin / marginal_coin_lot,
    );
    let marginal_pair = try_coin(marginal_lots * marginal_coin_lot)?;

    let reduced_coin_lot = bid_coin / bid_gcd as u128;
    let reduced_usd_lot = bid_usd / bid_gcd as u128;
    let exact_coin = (candidate_coin / reduced_coin_lot) * reduced_coin_lot;
    let bid_pair = try_coin(exact_coin)?;
    let mut best = marginal_pair;
    consider_larger_pair(&mut best, bid_pair);
    if best.is_some_and(|pair| pair == (candidate_coin, max_usd)) {
        return Ok(best);
    }

    // For reduced endpoints a/b < c/d, every strict interior p/q satisfies
    // determinant*q = b*(c*q - p*d) + d*(p*b - a*q) >= b + d. This constant-work
    // lower bound bypasses continued fractions when no interior denominator can fit.
    let interval_determinant = reduced_coin_lot
        .checked_mul(marginal_usd_lot)
        .and_then(|value| value.checked_sub(marginal_coin_lot * reduced_usd_lot))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if interval_determinant == 0 {
        return Ok(best);
    }
    // For a reduced candidate x/q above marginal a/b, let r = b*x - a*q. Bidder safety against
    // c/d requires d*r <= (b*c - a*d)*q, so every feasible r is bounded by determinant*max_usd/d.
    // When that bound is small, enumerate residue classes instead of descending one denominator at
    // a time. The upper Stern-Brocot parent p/s of a/b has b*p-a*s=1, hence every denominator for
    // excess r is congruent to r*s mod b. The largest budget-safe member of each class dominates
    // every smaller member, making this branch exact and cheap for narrow public intervals.
    if let Some(determinant_budget) = interval_determinant.checked_mul(max_usd) {
        let max_excess = determinant_budget / reduced_usd_lot;
        if max_excess <= MAX_EXCESS_RESIDUES {
            if max_excess > 0 {
                // For an integer marginal a/1, 1/0 is the omitted upper Stern-Brocot parent and
                // correctly places every excess in the sole denominator congruence class.
                let (parent_coin, parent_usd) = if marginal_usd_lot == 1 {
                    (1, 0)
                } else {
                    let marginal_basis = simplest_interval_fraction(
                        marginal_coin_lot as u64,
                        marginal_usd_lot as u64,
                        marginal_coin_lot as u64,
                        marginal_usd_lot as u64,
                    )?;
                    marginal_basis
                        .right_parent
                        .ok_or(ProgramError::InvalidAccountData)?
                };
                if marginal_usd_lot
                    .checked_mul(parent_coin)
                    .and_then(|value| {
                        value.checked_sub(marginal_coin_lot * parent_usd)
                    })
                    != Some(1)
                {
                    return Err(ProgramError::InvalidAccountData);
                }

                for excess in 1..=max_excess {
                    let coin_capacity = marginal_usd_lot
                        .checked_mul(candidate_coin)
                        .ok_or(ProgramError::ArithmeticOverflow)?;
                    if coin_capacity < excess {
                        continue;
                    }
                    let denominator_limit = core::cmp::min(
                        max_usd,
                        (coin_capacity - excess) / marginal_coin_lot,
                    );
                    let class = excess
                        .checked_mul(parent_usd)
                        .ok_or(ProgramError::ArithmeticOverflow)?
                        % marginal_usd_lot;
                    let denominator = if class == 0 {
                        (denominator_limit / marginal_usd_lot) * marginal_usd_lot
                    } else if class <= denominator_limit {
                        class
                            + ((denominator_limit - class) / marginal_usd_lot)
                                * marginal_usd_lot
                    } else {
                        0
                    };
                    if denominator == 0
                        || best.is_some_and(|pair| denominator < pair.1)
                        || reduced_usd_lot
                            .checked_mul(excess)
                            .ok_or(ProgramError::ArithmeticOverflow)?
                            > interval_determinant
                                .checked_mul(denominator)
                                .ok_or(ProgramError::ArithmeticOverflow)?
                    {
                        continue;
                    }
                    let coin = marginal_coin_lot
                        .checked_mul(denominator)
                        .and_then(|value| value.checked_add(excess))
                        .ok_or(ProgramError::ArithmeticOverflow)?
                        / marginal_usd_lot;
                    consider_larger_pair(&mut best, try_coin(coin)?);
                }
            }
            return Ok(best);
        }
    }
    let endpoint_den_sum = marginal_usd_lot
        .checked_add(reduced_usd_lot)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let min_interior_den = endpoint_den_sum / interval_determinant
        + u128::from(endpoint_den_sum % interval_determinant != 0);
    if min_interior_den > max_usd {
        return Ok(best);
    }

    let interior = simplest_interval_fraction(
        marginal_coin_lot as u64,
        marginal_usd_lot as u64,
        reduced_coin_lot as u64,
        reduced_usd_lot as u64,
    )?;
    if interior.den <= max_usd && interior.num <= bid_coin {
        let lots = core::cmp::min(max_usd / interior.den, bid_coin / interior.num);
        consider_larger_pair(&mut best, try_coin(lots * interior.num)?);

        for parent in [interior.left_parent, interior.right_parent]
            .into_iter()
            .flatten()
        {
            let (parent_num, parent_den) = parent;
            let mut steps = (max_usd - interior.den) / parent_den;

            // A parent outside the lower edge can be approached only until the resulting mediant
            // reaches that edge. The initial fraction itself is already inside the interval.
            let parent_below = parent_num
                .checked_mul(marginal_usd_lot)
                .ok_or(ProgramError::ArithmeticOverflow)?
                < marginal_coin_lot
                    .checked_mul(parent_den)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
            if parent_below {
                let headroom = interior
                    .num
                    .checked_mul(marginal_usd_lot)
                    .and_then(|value| {
                        value.checked_sub(marginal_coin_lot * interior.den)
                    })
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                let loss_per_step = marginal_coin_lot
                    .checked_mul(parent_den)
                    .and_then(|value| {
                        value.checked_sub(parent_num * marginal_usd_lot)
                    })
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                steps = core::cmp::min(steps, headroom / loss_per_step);
            }

            let parent_above = parent_num
                .checked_mul(reduced_usd_lot)
                .ok_or(ProgramError::ArithmeticOverflow)?
                > reduced_coin_lot
                    .checked_mul(parent_den)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
            if parent_above {
                let headroom = reduced_coin_lot
                    .checked_mul(interior.den)
                    .and_then(|value| {
                        value.checked_sub(interior.num * reduced_usd_lot)
                    })
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                let loss_per_step = parent_num
                    .checked_mul(reduced_usd_lot)
                    .and_then(|value| {
                        value.checked_sub(reduced_coin_lot * parent_den)
                    })
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                steps = core::cmp::min(steps, headroom / loss_per_step);
            }

            let ray_coin = interior
                .num
                .checked_add(
                    steps
                        .checked_mul(parent_num)
                        .ok_or(ProgramError::ArithmeticOverflow)?,
                )
                .ok_or(ProgramError::ArithmeticOverflow)?;
            consider_larger_pair(&mut best, try_coin(ray_coin)?);
        }
    }

    // The endpoint and continued-fraction candidates above seed a close lower bound before the
    // denominator projection. Once the descending projection reaches that denominator it
    // cannot improve the result, which keeps a full public book compute-bounded. The hard cap is a
    // final liveness guard; reaching it retains the established safe fallback instead of failing
    // otherwise executable auctions.
    let mut projected_usd = max_usd;
    for _ in 0..128 {
        if projected_usd == 0 || best.is_some_and(|pair| projected_usd < pair.1) {
            break;
        }
        let coin = projected_coin_for_usd(projected_usd)?;
        if coin == 0 {
            break;
        }
        if coin
            .checked_mul(marginal_usd)
            .ok_or(ProgramError::ArithmeticOverflow)?
            >= marginal_coin
                .checked_mul(projected_usd)
                .ok_or(ProgramError::ArithmeticOverflow)?
        {
            return Ok(Some((coin, projected_usd)));
        }
        let next_usd = mul_div_floor(coin, marginal_usd, marginal_coin)?;
        if next_usd >= projected_usd {
            break;
        }
        projected_usd = next_usd;
    }
    Ok(best)
}

fn as_u64(v: u128) -> Result<u64, ProgramError> {
    u64::try_from(v).map_err(|_| ProgramError::InvalidInstructionData)
}

// Build + invoke an spl-token transfer (`from` -> `to`), authorised by `authority`. With seeds the
// authority is a PDA (invoke_signed); without, it must be a transaction signer (invoke).
fn spl_transfer<'a>(
    token_program: &AccountInfo<'a>,
    from: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    amount: u64,
    seeds: Option<&[&[u8]]>,
) -> ProgramResult {
    let mut data = vec![TOKEN_IX_TRANSFER];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: *token_program.key,
        accounts: vec![
            AccountMeta::new(*from.key, false),
            AccountMeta::new(*to.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data,
    };
    let infos = [
        from.clone(),
        to.clone(),
        authority.clone(),
        token_program.clone(),
    ];
    match seeds {
        Some(s) => invoke_signed(&ix, &infos, &[s]),
        None => invoke(&ix, &infos),
    }
}

// Build + invoke an spl-token burn of `amount` from `account` (of `mint`), authorised by the PDA
// `authority` via `seeds`.
fn spl_burn_signed<'a>(
    token_program: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    amount: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    let mut data = vec![TOKEN_IX_BURN];
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: *token_program.key,
        accounts: vec![
            AccountMeta::new(*account.key, false),
            AccountMeta::new(*mint.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data,
    };
    invoke_signed(
        &ix,
        &[
            account.clone(),
            mint.clone(),
            authority.clone(),
            token_program.clone(),
        ],
        &[seeds],
    )
}

// Require + return the config's Squads default vault as the authoriser of a DAO-gated mutation.
fn require_squads_vault(squads_vault: &AccountInfo, config: &Config) -> ProgramResult {
    if !squads_vault.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *squads_vault.key != squads_default_vault(&config.squads_multisig) {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(())
}

fn load_coin_sink(
    account: &AccountInfo,
    coin_mint: &Pubkey,
) -> Result<spl_token::state::Account, ProgramError> {
    if account.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let state = spl_token::state::Account::unpack(&account.try_borrow_data()?)?;
    if state.state != spl_token::state::AccountState::Initialized || state.mint != *coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(state)
}

fn validate_coin_sink_configuration(account: &AccountInfo, coin_mint: &Pubkey) -> ProgramResult {
    let state = load_coin_sink(account, coin_mint)?;
    if state.delegate.is_some() || state.delegated_amount != 0 || state.close_authority.is_some() {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn validate_clean_owner_token_account(
    account: &AccountInfo,
    expected_owner: &Pubkey,
    expected_mint: &Pubkey,
) -> ProgramResult {
    if account.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let state = spl_token::state::Account::unpack(&account.try_borrow_data()?)?;
    if state.state != spl_token::state::AccountState::Initialized
        || state.owner != *expected_owner
        || state.mint != *expected_mint
        || state.delegate.is_some()
        || state.delegated_amount != 0
        || state.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

// init_book accounts: [squads_vault(signer, payer), config, book(w, init), book_escrow(pda),
//   coin_escrow, settlement_usd, holding, coin_mint, collateral_mint, system_program, coin_sink?]
// data: reserve_num (u128) || reserve_den (u128) || round_length (u64) || sink_mode (u8)
//   || bid_fee (u64) || sink_cutoff_slot? (u64; absent = no cutoff)
//
// Squads-vault-gated (timelock'd): pins the reserve, round length, COIN sink, binding mints and
// the canonical USD holding, and records the shared COIN-escrow + settlement-USD token accounts
// (owned by the book-escrow PDA, pre-created by the caller). The holding is the single
// twap_authority-owned account `execute` pulls surplus into and rolls over across rounds —
// pinning it here keeps the accumulated budget from fragmenting. Everything that drives the
// auction afterwards is permissionless.
fn process_init_book(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;
    let book_escrow = next_account_info(iter)?;
    let coin_escrow = next_account_info(iter)?;
    let settlement_usd = next_account_info(iter)?;
    let holding = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if data.len() != 49 && data.len() != 57 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let reserve_num = u128::from_le_bytes(data[..16].try_into().unwrap());
    let reserve_den = u128::from_le_bytes(data[16..32].try_into().unwrap());
    let round_length = u64::from_le_bytes(data[32..40].try_into().unwrap());
    let sink_mode = data[40];
    let bid_fee = u64::from_le_bytes(data[41..49].try_into().unwrap());
    let requested_sink_cutoff = if data.len() == 57 {
        u64::from_le_bytes(data[49..57].try_into().unwrap())
    } else {
        u64::MAX
    };
    if reserve_den == 0
        || round_length == 0
        || round_length.checked_mul(2).is_none()
        || sink_mode > SINK_SEND
    {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    require_squads_vault(squads_vault, &config)?;
    if *coin_mint.key != config.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }

    let (expected_escrow, escrow_bump) =
        Pubkey::find_program_address(&[BOOK_ESCROW_SEED, config_account.key.as_ref()], program_id);
    if *book_escrow.key != expected_escrow {
        return Err(ProgramError::InvalidSeeds);
    }
    // Require SPL Token ownership BEFORE unpacking each token account init_book PERSISTS into the book
    // (coin_escrow, settlement_usd, holding, coin_sink). Account::unpack verifies bytes, NOT the owning program,
    // so a non-SPL token-shaped account would pass the field checks. init_book is squads-vault-gated (unlike the
    // permissionless rd freeze / subledger init_pool where this same gap was an exploitable front-run brick), so
    // here it is fail-fast hardening: it stops a DAO mistake from binding a non-SPL account and permanently
    // bricking the auction (every place_bid/execute would then fail on the bound fake). Parity with
    // distribution:342 + the rd freeze + subledger init_pool fixes.
    if coin_escrow.owner != &spl_token::ID || settlement_usd.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let ce = spl_token::state::Account::unpack(&coin_escrow.try_borrow_data()?)?;
    if ce.state != spl_token::state::AccountState::Initialized
        || ce.owner != expected_escrow
        || ce.mint != *coin_mint.key
        || ce.amount != 0
        || ce.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let su = spl_token::state::Account::unpack(&settlement_usd.try_borrow_data()?)?;
    if su.state != spl_token::state::AccountState::Initialized
        || su.owner != expected_escrow
        || su.mint != *collateral_mint.key
        || su.amount != 0
        || su.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // The canonical USD holding must be a collateral token account owned by the twap_authority
    // (so percolator's WithdrawInsuranceLimited will pay into it during execute).
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let twap_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if holding.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let hs = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?;
    if hs.state != spl_token::state::AccountState::Initialized
        || hs.owner != twap_authority
        || hs.mint != *collateral_mint.key
        || hs.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // In SEND mode, validate + record the COIN sink (a COIN token account); BURN mode ignores it.
    let coin_sink_key = if sink_mode == SINK_SEND {
        let coin_sink = next_account_info(iter)?;
        // Same-mint markets make every custody account a valid COIN account. Keep the sink outside
        // all custody so bought COIN cannot be stranded under auction accounting. (finding BT)
        if *coin_sink.key == *coin_escrow.key
            || *coin_sink.key == *settlement_usd.key
            || *coin_sink.key == *holding.key
        {
            return Err(ProgramError::InvalidAccountData);
        }
        validate_coin_sink_configuration(coin_sink, coin_mint.key)?;
        *coin_sink.key
    } else {
        Pubkey::default()
    };

    let round_end = solana_program::clock::Clock::get()?
        .slot
        .checked_add(round_length)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let sink_cutoff_slot = if sink_mode == SINK_SEND {
        if requested_sink_cutoff < round_end {
            return Err(ProgramError::InvalidInstructionData);
        }
        requested_sink_cutoff
    } else {
        u64::MAX
    };

    let (expected_book, book_bump) =
        Pubkey::find_program_address(&[BOOK_SEED, config_account.key.as_ref()], program_id);
    if *book_account.key != expected_book {
        return Err(ProgramError::InvalidSeeds);
    }
    if book_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let bump_arr = [book_bump];
    let seeds: [&[u8]; 3] = [BOOK_SEED, config_account.key.as_ref(), &bump_arr];
    create_pda_robust(
        squads_vault,
        book_account,
        system_program,
        program_id,
        &seeds,
        BOOK_SIZE,
    )?;

    let mut d = book_account.try_borrow_mut_data()?;
    d[..8].copy_from_slice(&BOOK_DISC);
    d[BK_CONFIG..BK_CONFIG + 32].copy_from_slice(config_account.key.as_ref());
    d[BK_COIN_MINT..BK_COIN_MINT + 32].copy_from_slice(coin_mint.key.as_ref());
    d[BK_COLLATERAL_MINT..BK_COLLATERAL_MINT + 32].copy_from_slice(collateral_mint.key.as_ref());
    d[BK_COIN_ESCROW..BK_COIN_ESCROW + 32].copy_from_slice(coin_escrow.key.as_ref());
    d[BK_SETTLEMENT_USD..BK_SETTLEMENT_USD + 32].copy_from_slice(settlement_usd.key.as_ref());
    d[BK_COIN_SINK..BK_COIN_SINK + 32].copy_from_slice(coin_sink_key.as_ref());
    d[BK_HOLDING..BK_HOLDING + 32].copy_from_slice(holding.key.as_ref());
    d[BK_BID_FEE..BK_BID_FEE + 8].copy_from_slice(&bid_fee.to_le_bytes());
    d[BK_SINK_CUTOFF_SLOT..BK_SINK_CUTOFF_SLOT + 8]
        .copy_from_slice(&sink_cutoff_slot.to_le_bytes());
    book_wr_u128(&mut d, BK_RESERVE_NUM, reserve_num);
    book_wr_u128(&mut d, BK_RESERVE_DEN, reserve_den);
    d[BK_ROUND_LENGTH..BK_ROUND_LENGTH + 8].copy_from_slice(&round_length.to_le_bytes());
    d[BK_ROUND_END..BK_ROUND_END + 8].copy_from_slice(&round_end.to_le_bytes());
    d[BK_STATE] = BOOK_STATE_OPEN;
    d[BK_SINK_MODE] = sink_mode;
    d[BK_BOOK_BUMP] = book_bump;
    d[BK_ESCROW_BUMP] = escrow_bump;
    Ok(())
}

// set_reserve accounts: [squads_vault(signer), config, book(w)]
// data: reserve_num (u128) || reserve_den (u128)
fn process_set_reserve(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;

    if data.len() != 32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let reserve_num = u128::from_le_bytes(data[..16].try_into().unwrap());
    let reserve_den = u128::from_le_bytes(data[16..32].try_into().unwrap());
    if reserve_den == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if config_account.owner != program_id || book_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    require_squads_vault(squads_vault, &config)?;
    let book = load_book_header(&book_account.try_borrow_data()?)?;
    if book.config != *config_account.key {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut d = book_account.try_borrow_mut_data()?;
    book_wr_u128(&mut d, BK_RESERVE_NUM, reserve_num);
    book_wr_u128(&mut d, BK_RESERVE_DEN, reserve_den);
    Ok(())
}

// set_coin_sink accounts: [squads_vault(signer), config, book(w), coin_sink?]
// data: sink_mode (u8) || sink_cutoff_slot? (u64; absent = no cutoff)
//
// Futarchy-configurable: burn the bought COIN (mode 0) or send it to an account (mode 1, e.g. a
// DAO treasury). Squads-vault-gated.
fn process_set_coin_sink(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;

    if (data.len() != 1 && data.len() != 9) || data[0] > SINK_SEND {
        return Err(ProgramError::InvalidInstructionData);
    }
    let sink_mode = data[0];
    let requested_sink_cutoff = if data.len() == 9 {
        u64::from_le_bytes(data[1..9].try_into().unwrap())
    } else {
        u64::MAX
    };
    if config_account.owner != program_id || book_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    require_squads_vault(squads_vault, &config)?;
    let book = load_book_header(&book_account.try_borrow_data()?)?;
    if book.config != *config_account.key {
        return Err(ProgramError::InvalidAccountData);
    }
    let sink_key = if sink_mode == SINK_SEND {
        let coin_sink = next_account_info(iter)?;
        // Same-mint markets make every custody account a valid COIN account. Keep the sink outside
        // all custody so bought COIN cannot be stranded under auction accounting. (finding BT)
        if *coin_sink.key == book.coin_escrow
            || *coin_sink.key == book.settlement_usd
            || *coin_sink.key == book.holding
        {
            return Err(ProgramError::InvalidAccountData);
        }
        validate_coin_sink_configuration(coin_sink, &book.coin_mint)?;
        *coin_sink.key
    } else {
        Pubkey::default()
    };
    let mut d = book_account.try_borrow_mut_data()?;
    d[BK_SINK_MODE] = sink_mode;
    d[BK_COIN_SINK..BK_COIN_SINK + 32].copy_from_slice(sink_key.as_ref());
    let sink_cutoff_slot = if sink_mode == SINK_SEND {
        requested_sink_cutoff
    } else {
        u64::MAX
    };
    d[BK_SINK_CUTOFF_SLOT..BK_SINK_CUTOFF_SLOT + 8]
        .copy_from_slice(&sink_cutoff_slot.to_le_bytes());
    Ok(())
}

// set_bid_fee accounts: [squads_vault(signer), config, book(w)]
// data: bid_fee (u64) — the flat COIN amount burned on every place_bid (anti-spam). Squads-gated
// and monotonic nonincreasing after book initialization.
fn process_set_bid_fee(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;

    if data.len() != 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bid_fee = u64::from_le_bytes(data.try_into().unwrap());
    if config_account.owner != program_id || book_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    require_squads_vault(squads_vault, &config)?;
    let book = load_book_header(&book_account.try_borrow_data()?)?;
    if book.config != *config_account.key {
        return Err(ProgramError::InvalidAccountData);
    }
    // The bid wire has no user-supplied maximum fee. An increase could land ahead of an
    // already-signed bid and burn unrelated COIN from the bidder's canonical account.
    if bid_fee > book.bid_fee {
        return Err(ProgramError::InvalidInstructionData);
    }
    book_account.try_borrow_mut_data()?[BK_BID_FEE..BK_BID_FEE + 8]
        .copy_from_slice(&bid_fee.to_le_bytes());
    Ok(())
}

// place_bid accounts: [bidder(signer), config, book(w), book_escrow(pda), coin_escrow(w),
//   bidder_coin_src(w), usd_dest, coin_mint, collateral_mint, token_program, evict_coin_dest(w)?]
// data: coin_atoms (u128) || usdc_atoms (u128) || expected_round_end (u64)
//   || expected_evicted_bidder (pubkey) || expected_evicted_coin (u128)
//   || expected_evicted_usdc (u128)
//
// PERMISSIONLESS. The bidder escrows `coin_atoms` COIN, offering it for `usdc_atoms` USD (limit
// rate coin/usdc). The bid CANNOT be cancelled afterwards (anti-spoofing) — it only leaves the
// book early by being evicted by a STRICTLY better bid, which immediately refunds the evictee.
fn process_place_bid(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    use core::cmp::Ordering;
    let iter = &mut accounts.iter();
    let bidder = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;
    let book_escrow = next_account_info(iter)?;
    let coin_escrow = next_account_info(iter)?;
    let bidder_coin_src = next_account_info(iter)?;
    let usd_dest = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if data.len() != 104 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let coin_atoms = u128::from_le_bytes(data[..16].try_into().unwrap());
    let usdc_atoms = u128::from_le_bytes(data[16..32].try_into().unwrap());
    let expected_round_end = u64::from_le_bytes(data[32..40].try_into().unwrap());
    let expected_evicted_bidder = Pubkey::new_from_array(data[40..72].try_into().unwrap());
    let expected_evicted_coin = u128::from_le_bytes(data[72..88].try_into().unwrap());
    let expected_evicted_usdc = u128::from_le_bytes(data[88..104].try_into().unwrap());
    let expected_eviction = match (
        expected_evicted_bidder == Pubkey::default(),
        expected_evicted_coin,
        expected_evicted_usdc,
    ) {
        (true, 0, 0) => None,
        (false, coin, usdc) if coin > 0 && usdc > 0 => {
            Some((expected_evicted_bidder, coin, usdc))
        }
        _ => return Err(ProgramError::InvalidInstructionData),
    };
    if coin_atoms == 0 || usdc_atoms == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    // Both legs are token amounts and MUST fit u64 — this bounds the constant-time bid-vs-bid
    // cross-multiply (u64*u64 < 2^128) so a full book can never blow execute's compute budget
    // (finding AC). It also subsumes the old coin_atoms*usdc_atoms overflow check.
    let coin_atoms_u64 = as_u64(coin_atoms)?;
    let _ = as_u64(usdc_atoms)?;
    if !bidder.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id || book_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let book = load_book_header(&book_account.try_borrow_data()?)?;
    if book.config != *config_account.key
        || book.state != BOOK_STATE_OPEN
        || book.round_end != expected_round_end
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let now = solana_program::clock::Clock::get()?.slot;
    if now >= book.round_end {
        return Err(ProgramError::Custom(ERR_ROUND_ACTIVE));
    }
    if *coin_mint.key != book.coin_mint
        || *coin_mint.key != config.coin_mint
        || *collateral_mint.key != book.collateral_mint
        || *coin_escrow.key != book.coin_escrow
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let escrow_bump = [book.escrow_bump];
    let escrow_seeds: [&[u8]; 3] = [BOOK_ESCROW_SEED, config_account.key.as_ref(), &escrow_bump];
    let expected_escrow = Pubkey::create_program_address(&escrow_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *book_escrow.key != expected_escrow {
        return Err(ProgramError::InvalidSeeds);
    }
    // The source must cover the escrowed COIN plus the flat anti-spam bid fee (burned below).
    let need = coin_atoms_u64
        .checked_add(book.bid_fee)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let canonical_coin_refund = bidder_coin_ata(bidder.key, coin_mint.key);
    if *bidder_coin_src.key != canonical_coin_refund || bidder_coin_src.owner != &spl_token::ID {
        return Err(ProgramError::InvalidAccountData);
    }
    let src = spl_token::state::Account::unpack(&bidder_coin_src.try_borrow_data()?)?;
    if src.state != spl_token::state::AccountState::Initialized
        || src.owner != *bidder.key
        || src.mint != *coin_mint.key
        || src.amount < need
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let canonical_usd_dest = bidder_coin_ata(bidder.key, collateral_mint.key);
    if *usd_dest.key != canonical_usd_dest || usd_dest.owner != &spl_token::ID {
        return Err(ProgramError::InvalidAccountData);
    }
    let dest = spl_token::state::Account::unpack(&usd_dest.try_borrow_data()?)?;
    if dest.state != spl_token::state::AccountState::Initialized
        || dest.owner != *bidder.key
        || dest.mint != *collateral_mint.key
    {
        return Err(ProgramError::InvalidAccountData);
    }

    // Decide the target slot. One active bid per bidder; placement never cancels an existing bid.
    let mut evicted: Option<(u128, Pubkey)> = None;
    let slot_i = {
        let d = book_account.try_borrow_data()?;
        for i in 0..MAX_BIDS {
            let o = slot_off(i);
            if d[o + SL_OCCUPIED] == 1 && book_rd_key(&d, o + SL_BIDDER) == *bidder.key {
                return Err(ProgramError::InvalidArgument); // already has an active bid
            }
        }
        let mut free = None;
        for i in 0..MAX_BIDS {
            if d[slot_off(i) + SL_OCCUPIED] == 0 {
                free = Some(i);
                break;
            }
        }
        match free {
            Some(i) => {
                if expected_eviction.is_some() {
                    return Err(ProgramError::InvalidAccountData);
                }
                i
            }
            None => {
                // Book full: find the weakest (lowest-rate) bid; evict it only if the incoming bid
                // is STRICTLY better. (Linear worst-scan — the heap's extract-min at N=32.)
                let mut weakest = 0usize;
                for i in 1..MAX_BIDS {
                    let oi = slot_off(i);
                    let ow = slot_off(weakest);
                    if cmp_bid(
                        book_rd_u128(&d, oi + SL_COIN),
                        book_rd_u128(&d, oi + SL_USDC),
                        book_rd_u128(&d, ow + SL_COIN),
                        book_rd_u128(&d, ow + SL_USDC),
                    ) == Ordering::Less
                    {
                        weakest = i;
                    }
                }
                let ow = slot_off(weakest);
                let live_eviction = (
                    book_rd_key(&d, ow + SL_BIDDER),
                    book_rd_u128(&d, ow + SL_COIN),
                    book_rd_u128(&d, ow + SL_USDC),
                );
                // A full-book placement must commit to the exact bid it is authorized to replace.
                // Since every replacement is strictly rate-improving, that bid cannot leave and
                // later become the weakest with the same legs in this round. This prevents an older
                // signed placement from becoming admissible again after a newer retry is evicted.
                if expected_eviction != Some(live_eviction) {
                    return Err(ProgramError::InvalidAccountData);
                }
                if cmp_bid(
                    coin_atoms,
                    usdc_atoms,
                    live_eviction.1,
                    live_eviction.2,
                ) != Ordering::Greater
                {
                    return Err(ProgramError::InsufficientFunds); // book full and incoming not better
                }
                evicted = Some((live_eviction.1, live_eviction.0));
                weakest
            }
        }
    };

    // Refund only to a clean account solely owned by the recorded evictee. The canonical ATA's
    // authority state can change after placement, so pinning its key would either expose the refund
    // to a newly-added delegate or let a poisoned ATA block every better bid permanently.
    if let Some((evicted_coin, evicted_bidder)) = evicted {
        let evict_acct = next_account_info(iter)?;
        validate_clean_owner_token_account(evict_acct, &evicted_bidder, coin_mint.key)?;
        spl_transfer(
            token_program,
            coin_escrow,
            evict_acct,
            book_escrow,
            as_u64(evicted_coin)?,
            Some(&escrow_seeds),
        )?;
    }

    // Charge the flat anti-spam fee: BURN it from the bidder's COIN account (non-refundable, even
    // on eviction). The bidder signs for their own account.
    if book.bid_fee > 0 {
        let mut bd = vec![TOKEN_IX_BURN];
        bd.extend_from_slice(&book.bid_fee.to_le_bytes());
        invoke(
            &Instruction {
                program_id: *token_program.key,
                accounts: vec![
                    AccountMeta::new(*bidder_coin_src.key, false),
                    AccountMeta::new(*coin_mint.key, false),
                    AccountMeta::new_readonly(*bidder.key, true),
                ],
                data: bd,
            },
            &[
                bidder_coin_src.clone(),
                coin_mint.clone(),
                bidder.clone(),
                token_program.clone(),
            ],
        )?;
    }

    // Escrow the incoming bid's COIN (bidder signs for their own source account).
    spl_transfer(
        token_program,
        bidder_coin_src,
        coin_escrow,
        bidder,
        coin_atoms_u64,
        None,
    )?;

    let mut d = book_account.try_borrow_mut_data()?;
    let o = slot_off(slot_i);
    d[o + SL_OCCUPIED] = 1;
    d[o + SL_SETTLED] = 0;
    d[o + SL_BIDDER..o + SL_BIDDER + 32].copy_from_slice(bidder.key.as_ref());
    // Record both canonical ATAs as deterministic recovery hints. Payout paths still revalidate
    // current SPL authority state and may use another clean bidder-owned account, because delegates,
    // close authorities, freezes, and account closure can change after placement.
    d[o + SL_USD_DEST..o + SL_USD_DEST + 32]
        .copy_from_slice(canonical_usd_dest.as_ref());
    d[o + SL_COIN_ATA..o + SL_COIN_ATA + 32]
        .copy_from_slice(canonical_coin_refund.as_ref());
    book_wr_u128(&mut d, o + SL_COIN, coin_atoms);
    book_wr_u128(&mut d, o + SL_USDC, usdc_atoms);
    book_wr_u128(&mut d, o + SL_USD_OWED, 0);
    book_wr_u128(&mut d, o + SL_COIN_REFUND, 0);
    // Record when the bid was placed and the round it joined. Both identify the bid incarnation;
    // place_slot also keeps cancellation closed until 2*round_length slots pass.
    let now = solana_program::clock::Clock::get()?.slot;
    d[o + SL_PLACE_SLOT..o + SL_PLACE_SLOT + 8].copy_from_slice(&now.to_le_bytes());
    d[o + SL_PLACE_ROUND_END..o + SL_PLACE_ROUND_END + 8]
        .copy_from_slice(&book.round_end.to_le_bytes());
    Ok(())
}

// execute accounts: [cranker(signer), config(w), book(w), twap_authority(pda), market_slab(w),
//   percolator_vault(w), vault_authority, percolator_program, holding(w), settlement_usd(w),
//   book_escrow(pda), coin_escrow(w), coin_mint(w), token_program, coin_sink(w)?]
//
// PERMISSIONLESS, allowed once the round's slots have expired. The SOLE path that moves insurance:
//  1) surplus = live asset-0 insurance - reserved_floor (the principal counter);
//  2) pull the burn-share (surplus * buy_burn_bps) into the holding as the auction budget;
//  3) ratchet the retained share into reserved_floor (it stays in insurance and compounds);
//  4) clear the whole book at one marginal uniform (Dutch) price; burn OR send the bought COIN;
//  5) open the next round.
fn process_execute(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    use core::cmp::Ordering;
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let holding = next_account_info(iter)?;
    let settlement_usd = next_account_info(iter)?;
    let book_escrow = next_account_info(iter)?;
    let coin_escrow = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !cranker.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id || book_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let book = load_book_header(&book_account.try_borrow_data()?)?;
    if book.config != *config_account.key || book.state != BOOK_STATE_OPEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if *coin_mint.key != book.coin_mint
        || *coin_mint.key != config.coin_mint
        || *coin_escrow.key != book.coin_escrow
        || *settlement_usd.key != book.settlement_usd
        || *market_slab.key != config.market_slab
        || *percolator_program.key != config.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_auth = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_auth {
        return Err(ProgramError::InvalidSeeds);
    }
    if *vault_authority.key != perc_vault_authority(market_slab.key, percolator_program.key) {
        return Err(ProgramError::InvalidSeeds);
    }
    let escrow_bump = [book.escrow_bump];
    let escrow_seeds: [&[u8]; 3] = [BOOK_ESCROW_SEED, config_account.key.as_ref(), &escrow_bump];
    let expected_escrow = Pubkey::create_program_address(&escrow_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *book_escrow.key != expected_escrow {
        return Err(ProgramError::InvalidSeeds);
    }
    {
        let h = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?;
        // Pinned: only the book's canonical holding can be used, so the rolled-over budget never
        // fragments across different twap_authority-owned accounts.
        if *holding.key != book.holding
            || h.owner != expected_auth
            || h.mint != book.collateral_mint
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let su = spl_token::state::Account::unpack(&settlement_usd.try_borrow_data()?)?;
        if su.owner != expected_escrow {
            return Err(ProgramError::InvalidAccountData);
        }
        let ce = spl_token::state::Account::unpack(&coin_escrow.try_borrow_data()?)?;
        if ce.owner != expected_escrow || ce.mint != *coin_mint.key {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    // Round gate: a round must run for its full length before it can be executed.
    let clock_slot = solana_program::clock::Clock::get()?.slot;
    if clock_slot < book.round_end {
        return Err(ProgramError::Custom(ERR_ROUND_ACTIVE));
    }

    // 1) surplus and the 80/20 split. The retained share stays in insurance AND is ratcheted into
    //    the principal counter so it is protected and compounds; only the burn-share is pulled.
    let insurance_balance = percolator_accounting::read_asset_insurance_balance(
        &market_slab.try_borrow_data()?,
        config.market_0_domain as usize,
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;
    let insurance = insurance_balance.remaining_atoms;
    let live_asset_exposed = {
        let market_data = market_slab.try_borrow_data()?;
        percolator_accounting::market_is_live(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            && percolator_accounting::asset_has_position_or_loss_state(
                &market_data,
                config.market_0_domain as usize,
            )
            .map_err(|_| ProgramError::InvalidAccountData)?
    };
    let surplus = if live_asset_exposed {
        0
    } else {
        insurance.saturating_sub(config.reserved_floor)
    };
    // Pull the two external routes as one cumulative share before apportioning it. Independent
    // per-route floors let public crankers repeatedly ratchet odd atoms into insurance; independent
    // carries are also unsafe because both routes can mature in one round and exceed that round's
    // surplus. The combined carry keeps total_pull <= surplus, while the second carry apportions
    // only those already-bounded atoms between auction and savings.
    let external_bps = (config.surplus_buy_burn_bps as u128)
        .checked_add(config.base_unit_savings_bps as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let external_numerator = surplus
        .checked_mul(external_bps)
        .and_then(|value| {
            value.checked_add(config.external_surplus_remainder_bps as u128)
        })
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let total_pull = external_numerator / BPS_DENOMINATOR as u128;
    config.external_surplus_remainder_bps =
        (external_numerator % BPS_DENOMINATOR as u128) as u16;
    let burnable = if external_bps == 0 {
        config.auction_split_remainder_bps = 0;
        0
    } else {
        let auction_numerator = total_pull
            .checked_mul(config.surplus_buy_burn_bps as u128)
            .and_then(|value| value.checked_add(config.auction_split_remainder_bps as u128))
            .ok_or(ProgramError::ArithmeticOverflow)?;
        let allocation = auction_numerator / external_bps;
        config.auction_split_remainder_bps = (auction_numerator % external_bps) as u16;
        allocation
    };
    let savings = total_pull
        .checked_sub(burnable)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let retained = surplus
        .checked_sub(total_pull)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let floor_after = config
        .reserved_floor
        .checked_add(retained)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let withdrawal_plan = if live_asset_exposed || insurance < config.reserved_floor {
        // An unarmed sentinel or a realized loss leaves no complete floor to rebalance.
        // Preserve its domain provenance and retain the historical no-op execute path.
        percolator_accounting::InsuranceWithdrawalPlan::default()
    } else {
        percolator_accounting::plan_insurance_withdrawal_to_domains(
            insurance_balance,
            total_pull,
            percolator_accounting::balanced_insurance_domains(floor_after),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?
    };

    // Validate the optional savings destination before either insurance pull. The book holding is
    // also a valid twap-authority-owned collateral account, but aliasing it would merge the savings
    // share into the auction budget and let a permissionless crank spend both allocations.
    let savings_dest = if savings > 0 {
        let savings_dest = next_account_info(iter)?;
        if *savings_dest.key != config.base_unit_savings_account
            || savings_dest.key == holding.key
        {
            return Err(ProgramError::InvalidAccountData);
        }
        if savings_dest.owner != &spl_token::ID {
            return Err(ProgramError::IllegalOwner);
        }
        let sd = spl_token::state::Account::unpack(&savings_dest.try_borrow_data()?)?;
        if sd.mint != book.collateral_mint
            || sd.owner != expected_auth
            || sd.state != spl_token::state::AccountState::Initialized
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Some(savings_dest)
    } else {
        None
    };

    // 2) Pull both configured surplus shares through one asset-wide withdrawal, then restore the
    //    exact 50/50 floor with domain top-ups. Percolator consumes long before short, so pulling
    //    only the net amount would silently replace long-domain principal with short-domain surplus.
    if withdrawal_plan.gross_withdrawal != 0 {
        let plan = withdrawal_plan;
        if plan.redeposit != [0, 0]
            && percolator_accounting::read_asset_insurance_authority(
                &market_slab.try_borrow_data()?,
                config.market_0_domain as usize,
            )
            .map_err(|_| ProgramError::InvalidAccountData)?
                != twap_authority.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut ix_data = vec![PERC_IX_WITHDRAW_INSURANCE_ASSET];
        ix_data.extend_from_slice(&(config.market_0_domain as u16).to_le_bytes());
        ix_data.extend_from_slice(&plan.gross_withdrawal.to_le_bytes());
        invoke_signed(
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*twap_authority.key, true),
                    AccountMeta::new(*market_slab.key, false),
                    AccountMeta::new(*holding.key, false),
                    AccountMeta::new(*percolator_vault.key, false),
                    AccountMeta::new_readonly(*vault_authority.key, false),
                    AccountMeta::new_readonly(*token_program.key, false),
                ],
                data: ix_data,
            },
            &[
                twap_authority.clone(),
                market_slab.clone(),
                holding.clone(),
                percolator_vault.clone(),
                vault_authority.clone(),
                token_program.clone(),
                percolator_program.clone(),
            ],
            &[&auth_seeds],
        )?;

        for (domain_offset, amount) in plan.redeposit.into_iter().enumerate() {
            if amount == 0 {
                continue;
            }
            let domain = (config.market_0_domain as usize)
                .checked_mul(2)
                .and_then(|v| v.checked_add(domain_offset))
                .ok_or(ProgramError::ArithmeticOverflow)?;
            let domain = u16::try_from(domain).map_err(|_| ProgramError::ArithmeticOverflow)?;
            let mut ix_data = vec![PERC_IX_TOP_UP_INSURANCE_DOMAIN];
            ix_data.extend_from_slice(&domain.to_le_bytes());
            ix_data.extend_from_slice(&amount.to_le_bytes());
            invoke_signed(
                &Instruction {
                    program_id: *percolator_program.key,
                    accounts: vec![
                        AccountMeta::new_readonly(*twap_authority.key, true),
                        AccountMeta::new(*market_slab.key, false),
                        AccountMeta::new(*holding.key, false),
                        AccountMeta::new(*percolator_vault.key, false),
                        AccountMeta::new_readonly(*token_program.key, false),
                    ],
                    data: ix_data,
                },
                &[
                    twap_authority.clone(),
                    market_slab.clone(),
                    holding.clone(),
                    percolator_vault.clone(),
                    token_program.clone(),
                    percolator_program.clone(),
                ],
                &[&auth_seeds],
            )?;
        }
    }

    // 2b) The combined pull lands in the canonical holding. Route only the configured savings
    //     share onward; the auction's burn/buyback budget remains in holding exactly as before.
    if let Some(savings_dest) = savings_dest {
        let savings = u64::try_from(savings).map_err(|_| ProgramError::ArithmeticOverflow)?;
        invoke_signed(
            &spl_token::instruction::transfer(
                token_program.key,
                holding.key,
                savings_dest.key,
                twap_authority.key,
                &[],
                savings,
            )?,
            &[
                holding.clone(),
                savings_dest.clone(),
                twap_authority.clone(),
                token_program.clone(),
            ],
            &[&auth_seeds],
        )?;
    }
    // 3) ratchet the retained share into the principal counter.
    config.reserved_floor = floor_after;

    // 4) clear the book against the budget now in the holding.
    let budget = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?.amount as u128;
    let mut total_coin = 0u128;
    let mut total_usd = 0u128;
    let mut settled = false;
    {
        let mut d = book_account.try_borrow_mut_data()?;
        // a) eligible bids: occupied, positive, rate >= reserve.
        let mut idx = [0usize; MAX_BIDS];
        let mut bid_gcd = [0u64; MAX_BIDS];
        let mut n = 0usize;
        for i in 0..MAX_BIDS {
            let o = slot_off(i);
            if d[o + SL_OCCUPIED] != 1 {
                continue;
            }
            let c = book_rd_u128(&d, o + SL_COIN);
            let u = book_rd_u128(&d, o + SL_USDC);
            if c == 0 || u == 0 {
                continue;
            }
            if cmp_rate(c, u, book.reserve_num, book.reserve_den) == Ordering::Less {
                continue;
            }
            idx[n] = i;
            n += 1;
        }
        // b) sort eligible indices by rate, best (highest coin/usdc) first.
        for a in 1..n {
            let key = idx[a];
            let ko = slot_off(key);
            let kc = book_rd_u128(&d, ko + SL_COIN);
            let ku = book_rd_u128(&d, ko + SL_USDC);
            let mut b = a;
            while b > 0 {
                let po = slot_off(idx[b - 1]);
                if cmp_bid(
                    book_rd_u128(&d, po + SL_COIN),
                    book_rd_u128(&d, po + SL_USDC),
                    kc,
                    ku,
                ) == Ordering::Less
                {
                    idx[b] = idx[b - 1];
                    b -= 1;
                } else {
                    break;
                }
            }
            idx[b] = key;
        }
        // c) Walk best->worst using nominal USD allocations; the last executable allocation sets
        //    the uniform marginal price. A bid can pass its own-rate preflight but become wholly
        //    infeasible only after a lower bid sets that price. Exclude every such fully refunded
        //    bid and recompute so it cannot keep budget away from lower executable bids. Integer
        //    payout rounding that still produces a trade cannot change the marginal price. Once the
        //    marginal is stable, its aggregate remainder may only enlarge allocations which already
        //    executed at that same price. Every retry excludes at least one of the fixed 32 slots.
        let mut excluded = vec![false; MAX_BIDS];
        let mut nominal_allocations = vec![0u128; MAX_BIDS];
        let mut stable_allocations = vec![None; MAX_BIDS];
        let mut has_stable = false;
        let mut stable_marginal_rank = None;
        for _ in 0..=MAX_BIDS {
            nominal_allocations.fill(0);
            let mut remaining = budget;
            let mut marginal = None;
            for k in 0..n {
                let i = idx[k];
                if remaining == 0 {
                    break;
                }
                if excluded[i] {
                    continue;
                }
                let o = slot_off(i);
                let c = book_rd_u128(&d, o + SL_COIN);
                let u = book_rd_u128(&d, o + SL_USDC);
                let nominal_usd = core::cmp::min(remaining, u);
                // At the bid's own rate, the floored COIN leg is bidder-safe exactly when it is a
                // multiple of the reduced COIN lot. Preserve that maximum nominal allocation. If
                // it is not exact, use the largest smaller reduced-ratio lot that fits. Cache the
                // GCD so bounded repricing retries cannot repeat attacker-controlled work.
                if bid_gcd[i] == 0 {
                    bid_gcd[i] = gcd_u64(c as u64, u as u64);
                }
                let lot_coin = c as u64 / bid_gcd[i];
                let lot_usd = u / bid_gcd[i] as u128;
                let floored_coin = mul_div_floor(nominal_usd, c, u)? as u64;
                let allocation = if floored_coin != 0 && floored_coin % lot_coin == 0 {
                    nominal_usd
                } else {
                    let exact_usd = (nominal_usd / lot_usd) * lot_usd;
                    if exact_usd == 0 {
                        continue;
                    }
                    exact_usd
                };
                nominal_allocations[i] = allocation;
                remaining -= allocation;
                marginal = Some((i, k));
            }

            let Some((marginal_slot, marginal_rank)) = marginal else {
                break;
            };
            let mo = slot_off(marginal_slot);
            let cm = book_rd_u128(&d, mo + SL_COIN);
            let um = book_rd_u128(&d, mo + SL_USDC);
            stable_allocations.fill(None);
            let mut retry = false;
            let mut candidate_coin = 0u128;
            let mut candidate_usd = 0u128;
            for i in 0..MAX_BIDS {
                let nominal_usd = nominal_allocations[i];
                if nominal_usd == 0 {
                    continue;
                }
                let o = slot_off(i);
                let c = book_rd_u128(&d, o + SL_COIN);
                let u = book_rd_u128(&d, o + SL_USDC);
                if let Some((coin_i, executable_usd)) =
                    executable_integer_pair(nominal_usd, cm, um, c, u)?
                {
                    stable_allocations[i] = Some((coin_i, executable_usd));
                    candidate_coin = candidate_coin
                        .checked_add(coin_i)
                        .ok_or(ProgramError::ArithmeticOverflow)?;
                    candidate_usd = candidate_usd
                        .checked_add(executable_usd)
                        .ok_or(ProgramError::ArithmeticOverflow)?;
                } else {
                    excluded[i] = true;
                    retry = true;
                }
            }
            if !retry {
                total_coin = candidate_coin;
                total_usd = candidate_usd;
                has_stable = true;
                stable_marginal_rank = Some(marginal_rank);
                break;
            }
        }

        // Integer reconciliation can leave each executable bid consuming less actual USD than its
        // nominal allocation, or leave additional bidder-safe COIN available for the same floored
        // USD payment. Combine remainders into additional whole lots and maximize equal-USD fills
        // without changing the stable marginal. This may enlarge an existing allocation or admit a
        // previously unallocated bid that is ranked above the marginal (for example, its lot did not
        // fit the nominal remainder) or exactly equal to it. Lower-price bids remain ineligible.
        // Mutating the existing fixed-size allocation vector avoids attacker-amplified heap use;
        // cached GCDs bound each fallback. Continue after the USD remainder reaches zero because
        // later allocated bids may still increase COIN without increasing their payment.
        if has_stable {
            let marginal_rank = stable_marginal_rank.ok_or(ProgramError::InvalidAccountData)?;
            let marginal_slot = idx[marginal_rank];
            let mo = slot_off(marginal_slot);
            let cm = book_rd_u128(&d, mo + SL_COIN);
            let um = book_rd_u128(&d, mo + SL_USDC);
            let marginal_gcd = gcd_u64(cm as u64, um as u64);
            let mut remaining = budget - total_usd;

            for (rank, &i) in idx[..n].iter().enumerate() {
                let o = slot_off(i);
                let c = book_rd_u128(&d, o + SL_COIN);
                let u = book_rd_u128(&d, o + SL_USDC);
                if rank > marginal_rank
                    && cmp_bid(c, u, cm, um) != core::cmp::Ordering::Equal
                {
                    continue;
                }
                let (old_coin, old_usd) = stable_allocations[i].unwrap_or((0, 0));
                if old_coin == c {
                    continue;
                }
                if bid_gcd[i] == 0 {
                    bid_gcd[i] = gcd_u64(c as u64, u as u64);
                }
                let allocation_budget = old_usd
                    .checked_add(remaining)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                let Some((new_coin, new_usd)) = max_executable_integer_pair(
                    allocation_budget,
                    cm,
                    um,
                    c,
                    u,
                    bid_gcd[i],
                    marginal_gcd,
                )?
                else {
                    continue;
                };
                if new_coin <= old_coin || new_usd < old_usd {
                    continue;
                }
                let added_usd = new_usd - old_usd;
                if added_usd > remaining {
                    return Err(ProgramError::InvalidAccountData);
                }
                stable_allocations[i] = Some((new_coin, new_usd));
                total_coin = total_coin
                    .checked_add(new_coin - old_coin)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                total_usd = total_usd
                    .checked_add(added_usd)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                remaining -= added_usd;
            }
        }

        // A higher-ranked bid can be absent from the nominal walk when its own reduced lot does
        // not fit, even though it is executable at the lower final marginal. If the existing
        // winners consume the full budget, the remainder pass above cannot reconsider that bid.
        // Replay final-price allocations in priority order while reserving one exact marginal lot;
        // adopt the bounded alternative only when it spends more USD, or spends the same USD for
        // more COIN. This preserves the chosen marginal and cannot displace it with a better bid.
        if has_stable {
            let marginal_rank = stable_marginal_rank.ok_or(ProgramError::InvalidAccountData)?;
            let skipped_priority_rank = idx[..marginal_rank]
                .iter()
                .position(|&i| stable_allocations[i].is_none());
            if let Some(replay_start_rank) = skipped_priority_rank {
                let marginal_slot = idx[marginal_rank];
                let mo = slot_off(marginal_slot);
                let cm = book_rd_u128(&d, mo + SL_COIN);
                let um = book_rd_u128(&d, mo + SL_USDC);
                let marginal_gcd = gcd_u64(cm as u64, um as u64);
                let marginal_lot_usd = um / marginal_gcd as u128;
                let mut replay_allocations = stable_allocations.clone();
                let mut replay_coin = total_coin;
                let mut replay_usd = total_usd;
                for &i in &idx[replay_start_rank..n] {
                    if let Some((coin_i, usd_i)) = replay_allocations[i].take() {
                        replay_coin = replay_coin
                            .checked_sub(coin_i)
                            .ok_or(ProgramError::ArithmeticOverflow)?;
                        replay_usd = replay_usd
                            .checked_sub(usd_i)
                            .ok_or(ProgramError::ArithmeticOverflow)?;
                    }
                }
                let mut replay_remaining = budget
                    .checked_sub(replay_usd)
                    .ok_or(ProgramError::ArithmeticOverflow)?;
                let mut replay_has_marginal = false;

                for (suffix_rank, &i) in idx[replay_start_rank..n].iter().enumerate() {
                    let rank = replay_start_rank + suffix_rank;
                    let o = slot_off(i);
                    let c = book_rd_u128(&d, o + SL_COIN);
                    let u = book_rd_u128(&d, o + SL_USDC);
                    if rank > marginal_rank
                        && cmp_bid(c, u, cm, um) != core::cmp::Ordering::Equal
                    {
                        continue;
                    }
                    let allocation_budget = if rank < marginal_rank {
                        replay_remaining.saturating_sub(marginal_lot_usd)
                    } else {
                        replay_remaining
                    };
                    if allocation_budget == 0 {
                        continue;
                    }
                    if bid_gcd[i] == 0 {
                        bid_gcd[i] = gcd_u64(c as u64, u as u64);
                    }
                    let Some((coin_i, usd_i)) = max_executable_integer_pair(
                        allocation_budget,
                        cm,
                        um,
                        c,
                        u,
                        bid_gcd[i],
                        marginal_gcd,
                    )?
                    else {
                        continue;
                    };
                    if usd_i > replay_remaining {
                        return Err(ProgramError::InvalidAccountData);
                    }
                    replay_allocations[i] = Some((coin_i, usd_i));
                    replay_coin = replay_coin
                        .checked_add(coin_i)
                        .ok_or(ProgramError::ArithmeticOverflow)?;
                    replay_usd = replay_usd
                        .checked_add(usd_i)
                        .ok_or(ProgramError::ArithmeticOverflow)?;
                    replay_remaining -= usd_i;
                    if cmp_bid(c, u, cm, um) == core::cmp::Ordering::Equal {
                        replay_has_marginal = true;
                    }
                    if replay_remaining == 0 {
                        break;
                    }
                }

                if replay_has_marginal
                    && (replay_usd > total_usd
                        || (replay_usd == total_usd && replay_coin > total_coin))
                {
                    stable_allocations = replay_allocations;
                    total_coin = replay_coin;
                    total_usd = replay_usd;
                }
            }
        }

        if has_stable {
            // d) Commit the stable uniform-price result. Every unfilled or excluded bid receives
            //    its complete COIN refund; only executable pairs contribute to settlement totals.
            for i in 0..MAX_BIDS {
                let o = slot_off(i);
                if d[o + SL_OCCUPIED] != 1 {
                    continue;
                }
                let c = book_rd_u128(&d, o + SL_COIN);
                if let Some((coin_i, executable_usd)) = stable_allocations[i] {
                    let refund = c
                        .checked_sub(coin_i)
                        .ok_or(ProgramError::ArithmeticOverflow)?;
                    book_wr_u128(&mut d, o + SL_USD_OWED, executable_usd);
                    book_wr_u128(&mut d, o + SL_COIN_REFUND, refund);
                } else {
                    book_wr_u128(&mut d, o + SL_USD_OWED, 0);
                    book_wr_u128(&mut d, o + SL_COIN_REFUND, c);
                }
                d[o + SL_SETTLED] = 1;
            }
        }
        // Settle normally when COIN was bought. If positive budget and reserve-eligible bids exist
        // but every possible marginal allocation is integer-infeasible, settle the book for
        // refunds only. Rolling that state would preserve a full higher-rate Sybil book forever:
        // a lower executable bid cannot enter because eviction requires a strictly better rate.
        if total_coin > 0 && total_usd > 0 {
            d[BK_STATE] = BOOK_STATE_SETTLED;
            settled = true;
        } else if budget > 0 && n > 0 && !has_stable {
            for i in 0..MAX_BIDS {
                let o = slot_off(i);
                if d[o + SL_OCCUPIED] == 1 {
                    let c = book_rd_u128(&d, o + SL_COIN);
                    book_wr_u128(&mut d, o + SL_USD_OWED, 0);
                    book_wr_u128(&mut d, o + SL_COIN_REFUND, c);
                    d[o + SL_SETTLED] = 1;
                }
            }
            d[BK_STATE] = BOOK_STATE_SETTLED;
        } else {
            // A zero-budget or no-eligible-bid roll keeps commitments live. Fully restore every
            // provisional payout field so each bid is byte-identical to its pre-execute state for
            // subsequent cancel, eviction, and settlement paths.
            for i in 0..MAX_BIDS {
                let o = slot_off(i);
                if d[o + SL_OCCUPIED] == 1 {
                    book_wr_u128(&mut d, o + SL_USD_OWED, 0);
                    book_wr_u128(&mut d, o + SL_COIN_REFUND, 0);
                    d[o + SL_SETTLED] = 0;
                }
            }
        }
        // Open the next round regardless.
        let next_end = clock_slot
            .checked_add(book.round_length)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        d[BK_ROUND_END..BK_ROUND_END + 8].copy_from_slice(&next_end.to_le_bytes());
    }

    // 5) split the bought COIN per the 4-way economics: retain buyback_bps (of the bought COIN) to the
    //    configured COIN sink (recycled to governance), BURN the rest. `sink_mode == SINK_SEND` is the
    //    "a coin_sink is configured" gate (set via init_book / set_coin_sink); `buyback_bps` (set via
    //    set_economics) is the fraction. With no sink (BURN mode) OR buyback_bps == 0, the whole bought
    //    amount is burned, exactly as before. Then move the spent USD to the settlement account.
    if settled {
        let sink_active = book.sink_mode == SINK_SEND && clock_slot <= book.sink_cutoff_slot;
        let requested_to_sink = if sink_active {
            let split_numerator = total_coin
                .checked_mul(config.buyback_bps as u128)
                .and_then(|value| value.checked_add(config.buyback_remainder_bps as u128))
                .ok_or(ProgramError::ArithmeticOverflow)?;
            config.buyback_remainder_bps =
                (split_numerator % BPS_DENOMINATOR as u128) as u16;
            split_numerator / BPS_DENOMINATOR as u128
        } else {
            0
        };
        let mut sink_for_transfer = None;
        let mut to_sink = 0;
        if book.sink_mode == SINK_SEND {
            // The coin_sink trailing account is always supplied in SINK_SEND mode (account ordering is
            // independent of the runtime buyback fraction). The key remains exact, so a cranker cannot
            // select the fallback. If the configured external account was closed or otherwise became
            // invalid, burn its share instead of letting that public mutation stall every round.
            let coin_sink = next_account_info(iter)?;
            if *coin_sink.key != book.coin_sink {
                return Err(ProgramError::InvalidAccountData);
            }
            if requested_to_sink > 0 && load_coin_sink(coin_sink, &book.coin_mint).is_ok() {
                to_sink = requested_to_sink;
                sink_for_transfer = Some(coin_sink);
            }
        }
        let to_burn = total_coin
            .checked_sub(to_sink)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        if let Some(coin_sink) = sink_for_transfer {
            spl_transfer(
                token_program,
                coin_escrow,
                coin_sink,
                book_escrow,
                as_u64(to_sink)?,
                Some(&escrow_seeds),
            )?;
        }
        if to_burn > 0 {
            spl_burn_signed(
                token_program,
                coin_escrow,
                coin_mint,
                book_escrow,
                as_u64(to_burn)?,
                &escrow_seeds,
            )?;
        }
        spl_transfer(
            token_program,
            holding,
            settlement_usd,
            twap_authority,
            as_u64(total_usd)?,
            Some(&auth_seeds),
        )?;
    }

    config.serialize(&mut config_account.try_borrow_mut_data()?)?;
    Ok(())
}

// claim accounts: [cranker(signer), config, book(w), book_escrow(pda), settlement_usd(w),
//   coin_escrow(w), usd_dest(w), coin_ata(w), token_program]
// data: slot_index (u8)
//
// PERMISSIONLESS (no bidder signature), so anyone may crank every claim and reopen the book. USD
// may go to any clean token account owned by the recorded bidder, allowing recovery if a canonical
// ATA is frozen, closed, or delegated after placement. The same rule protects unsold COIN principal.
fn process_claim(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;
    let book_escrow = next_account_info(iter)?;
    let settlement_usd = next_account_info(iter)?;
    let coin_escrow = next_account_info(iter)?;
    let usd_dest = next_account_info(iter)?;
    let coin_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if data.len() != 1 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let slot_index = data[0] as usize;
    if slot_index >= MAX_BIDS {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !cranker.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id || book_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    validate_exit_config(&config_account.try_borrow_data()?)?;
    let (book, book_layout) = load_exit_book_header(&book_account.try_borrow_data()?)?;
    if book.config != *config_account.key
        || *settlement_usd.key != book.settlement_usd
        || *coin_escrow.key != book.coin_escrow
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let escrow_bump = [book.escrow_bump];
    let escrow_seeds: [&[u8]; 3] = [BOOK_ESCROW_SEED, config_account.key.as_ref(), &escrow_bump];
    let expected_escrow = Pubkey::create_program_address(&escrow_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *book_escrow.key != expected_escrow {
        return Err(ProgramError::InvalidSeeds);
    }

    let (usd_owed, coin_refund, bidder_key) = {
        let d = book_account.try_borrow_data()?;
        let o = book_layout.slot_off(slot_index);
        if d[o + SL_OCCUPIED] != 1 || d[o + SL_SETTLED] != 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        (
            book_rd_u128(&d, o + SL_USD_OWED),
            book_rd_u128(&d, o + SL_COIN_REFUND),
            book_rd_key(&d, o + SL_BIDDER),
        )
    };
    validate_clean_owner_token_account(usd_dest, &bidder_key, &book.collateral_mint)?;
    validate_clean_owner_token_account(coin_ata, &bidder_key, &book.coin_mint)?;
    if usd_owed > 0 {
        spl_transfer(
            token_program,
            settlement_usd,
            usd_dest,
            book_escrow,
            as_u64(usd_owed)?,
            Some(&escrow_seeds),
        )?;
    }
    if coin_refund > 0 {
        spl_transfer(
            token_program,
            coin_escrow,
            coin_ata,
            book_escrow,
            as_u64(coin_refund)?,
            Some(&escrow_seeds),
        )?;
    }

    let mut d = book_account.try_borrow_mut_data()?;
    let o = book_layout.slot_off(slot_index);
    for b in d[o..o + book_layout.slot_size].iter_mut() {
        *b = 0;
    }
    let mut any = false;
    for i in 0..MAX_BIDS {
        if d[book_layout.slot_off(i) + SL_OCCUPIED] == 1 {
            any = true;
            break;
        }
    }
    if !any {
        d[BK_STATE] = BOOK_STATE_OPEN;
        // Reset the round timer so the REOPENED round runs its full competition window from NOW. `execute`
        // sets round_end at SETTLE (= settle_slot + round_length) for "the next round", but the next round
        // does not actually start until the settled book's claims drain. If that drain takes longer than
        // round_length, the reopened round would inherit a round_end already in the past and be INSTANTLY
        // executable — letting a bidder (who could engineer it by delaying their own claim) place a bid and
        // crank execute before any competitor reacts, clearing ALONE and skipping the round_length window
        // that pushes the uniform clearing price above the reserve floor. Re-anchor round_end to the actual
        // round start (this reopen) so every round ages its full length first.
        let round_length = book_rd_u64(&d, BK_ROUND_LENGTH);
        let now = solana_program::clock::Clock::get()?.slot;
        let next_end = now.saturating_add(round_length);
        d[BK_ROUND_END..BK_ROUND_END + 8].copy_from_slice(&next_end.to_le_bytes());
    }
    Ok(())
}

// cancel_bid accounts: [bidder(signer), config, book(w), book_escrow(pda), coin_escrow(w),
//   coin_ata(w), token_program]
// data (current books): slot_index (u8) || place_slot (u64) || place_round_end (u64)
//   || coin_atoms (u128) || usdc_atoms (u128)
// data (exit-only legacy books without a cancel clock): slot_index (u8)
//
// Reclaim an UNSETTLED bid's escrowed COIN. Bidder-signed and gated until 2*round_length slots
// have elapsed. That cooldown prevents a last-second cancel from manipulating a pending execute.
// A settled bid is resolved through `claim` instead.
// Only the escrowed `coin_atoms` is returned — the flat anti-spam fee was burned up front at
// placement and is never refunded, so cancelling still costs the bidder the fee.
fn process_cancel_bid(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let bidder = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let book_account = next_account_info(iter)?;
    let book_escrow = next_account_info(iter)?;
    let coin_escrow = next_account_info(iter)?;
    let coin_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let (slot_index, expected_bid) = match data.len() {
        1 => (data[0] as usize, None),
        49 => (
            data[0] as usize,
            Some((
                u64::from_le_bytes(data[1..9].try_into().unwrap()),
                u64::from_le_bytes(data[9..17].try_into().unwrap()),
                u128::from_le_bytes(data[17..33].try_into().unwrap()),
                u128::from_le_bytes(data[33..49].try_into().unwrap()),
            )),
        ),
        _ => return Err(ProgramError::InvalidInstructionData),
    };
    if slot_index >= MAX_BIDS {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !bidder.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id || book_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    validate_exit_config(&config_account.try_borrow_data()?)?;
    let (book, book_layout) = load_exit_book_header(&book_account.try_borrow_data()?)?;
    // Current slots are reusable. Require the owner signature to commit to every immutable field
    // that identifies the intended bid, so a withheld cancellation cannot act on a later slot
    // incarnation. Pre-clock books are exit-only and retain their historical one-byte wire.
    if book_layout.has_cancel_clock() != expected_bid.is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if book.config != *config_account.key || *coin_escrow.key != book.coin_escrow {
        return Err(ProgramError::InvalidAccountData);
    }
    let escrow_bump = [book.escrow_bump];
    let escrow_seeds: [&[u8]; 3] = [BOOK_ESCROW_SEED, config_account.key.as_ref(), &escrow_bump];
    let expected_escrow = Pubkey::create_program_address(&escrow_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *book_escrow.key != expected_escrow {
        return Err(ProgramError::InvalidSeeds);
    }

    let coin_atoms = {
        let d = book_account.try_borrow_data()?;
        let o = book_layout.slot_off(slot_index);
        if d[o + SL_OCCUPIED] != 1 || d[o + SL_SETTLED] != 0 {
            return Err(ProgramError::InvalidAccountData); // empty, or settled (use claim)
        }
        if book_rd_key(&d, o + SL_BIDDER) != *bidder.key {
            return Err(ProgramError::IllegalOwner); // only the bidder may cancel their own bid
        }
        if book_layout.has_cancel_clock() {
            let expected_bid = expected_bid.ok_or(ProgramError::InvalidInstructionData)?;
            let live_bid = (
                book_rd_u64(&d, o + SL_PLACE_SLOT),
                book_rd_u64(&d, o + SL_PLACE_ROUND_END),
                book_rd_u128(&d, o + SL_COIN),
                book_rd_u128(&d, o + SL_USDC),
            );
            if live_bid != expected_bid {
                return Err(ProgramError::InvalidAccountData);
            }
            // Anti-spoof commitment (issue #28): a current-format bid is committed until it is
            // settled or the full 2*round_length aging window elapses. Pre-cancel generations have
            // no clock, but are exit-only under the upgraded binary, so cancelling them cannot
            // manipulate any future execute. Do not shortcut current bids on a round_end change: a
            // permissionless no-op roll advances round_end without clearing the committed bid.
            let place_slot = book_rd_u64(&d, o + SL_PLACE_SLOT);
            let now = solana_program::clock::Clock::get()?.slot;
            let cooldown_slots = book
                .round_length
                .checked_mul(2)
                .ok_or(ProgramError::ArithmeticOverflow)?;
            let cooldown_end = place_slot
                .checked_add(cooldown_slots)
                .ok_or(ProgramError::ArithmeticOverflow)?;
            if now < cooldown_end {
                return Err(ProgramError::Custom(ERR_ROUND_ACTIVE));
            }
        }
        book_rd_u128(&d, o + SL_COIN)
    };
    validate_clean_owner_token_account(coin_ata, bidder.key, &book.coin_mint)?;

    if coin_atoms > 0 {
        spl_transfer(
            token_program,
            coin_escrow,
            coin_ata,
            book_escrow,
            as_u64(coin_atoms)?,
            Some(&escrow_seeds),
        )?;
    }
    let mut d = book_account.try_borrow_mut_data()?;
    let o = book_layout.slot_off(slot_index);
    for b in d[o..o + book_layout.slot_size].iter_mut() {
        *b = 0;
    }
    Ok(())
}

// shutdown accounts: [squads_vault(signer), config, twap_authority(pda), holding(w), dest(w),
//   token_program]
//
// Squads-vault-gated wind-down: sweep ALL of the TWAP's accumulated USD (the unspent buy/burn
// budget in the holding) to a DAO-supplied destination. The TWAP normally KEEPS its dollars and
// adds more each round; this is the only path that takes them back out.
fn process_shutdown(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let holding = next_account_info(iter)?;
    let dest = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    require_squads_vault(squads_vault, &config)?;
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 3] = [TWAP_AUTHORITY_SEED, config_account.key.as_ref(), &auth_bump];
    let expected_auth = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_auth {
        return Err(ProgramError::InvalidSeeds);
    }
    let h = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?;
    if h.owner != expected_auth {
        return Err(ProgramError::InvalidAccountData);
    }
    let dd = spl_token::state::Account::unpack(&dest.try_borrow_data()?)?;
    if dd.mint != h.mint {
        return Err(ProgramError::InvalidAccountData);
    }
    if h.amount > 0 {
        spl_transfer(
            token_program,
            holding,
            dest,
            twap_authority,
            h.amount,
            Some(&auth_seeds),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmp_rate_orders_by_coin_per_usd() {
        use core::cmp::Ordering;
        // 3 COIN/USD beats 2 COIN/USD (more COIN per dollar = better for the protocol).
        assert_eq!(cmp_rate(3, 1, 2, 1), Ordering::Greater);
        assert_eq!(cmp_rate(2, 1, 3, 1), Ordering::Less);
        // Equal rates expressed with different denominators compare equal.
        assert_eq!(cmp_rate(6, 2, 9, 3), Ordering::Equal);
        // Fine-grained, overflow-safe comparison via continued fractions.
        assert_eq!(
            cmp_rate(1_000_001, 1_000_000, 1_000_000, 1_000_000),
            Ordering::Greater
        );
        assert_eq!(cmp_rate(u128::MAX, 3, u128::MAX, 4), Ordering::Greater);
    }

    #[test]
    fn simplest_interval_fraction_exposes_bounded_interior_parent_rays() {
        let fraction = simplest_interval_fraction(11, 24, 227, 480).unwrap();
        assert_eq!((fraction.num, fraction.den), (6, 13));
        assert_eq!(fraction.left_parent, Some((5, 11)));
        assert_eq!(fraction.right_parent, Some((1, 2)));

        // Two right-parent steps produce the largest candidate used by the real-SBF regression.
        let right = fraction.right_parent.unwrap();
        assert_eq!(
            (fraction.num + 2 * right.0, fraction.den + 2 * right.1),
            (8, 17)
        );
    }

    #[test]
    fn simplest_interval_fraction_handles_equal_and_integer_edges() {
        let equal = simplest_interval_fraction(2, 3, 2, 3).unwrap();
        assert_eq!((equal.num, equal.den), (2, 3));

        let integer = simplest_interval_fraction(5, 3, 2, 1).unwrap();
        assert_eq!((integer.num, integer.den), (2, 1));
        assert!(integer.right_parent.is_none());

        assert!(simplest_interval_fraction(3, 2, 4, 3).is_err());
        assert!(simplest_interval_fraction(1, 0, 2, 1).is_err());
    }

    #[test]
    fn narrow_excess_solver_matches_exhaustive_maximum_pair() {
        for marginal_coin in 1u128..=12 {
            for marginal_usd in 1u128..=12 {
                let marginal_gcd = gcd_u64(marginal_coin as u64, marginal_usd as u64);
                let marginal_coin_lot = marginal_coin / marginal_gcd as u128;
                let marginal_usd_lot = marginal_usd / marginal_gcd as u128;

                for bid_coin in 1u128..=12 {
                    for bid_usd in 1u128..=12 {
                        if cmp_bid(
                            marginal_coin,
                            marginal_usd,
                            bid_coin,
                            bid_usd,
                        ) == core::cmp::Ordering::Greater
                        {
                            continue;
                        }
                        let bid_gcd = gcd_u64(bid_coin as u64, bid_usd as u64);
                        let bid_coin_lot = bid_coin / bid_gcd as u128;
                        let bid_usd_lot = bid_usd / bid_gcd as u128;
                        let determinant = bid_coin_lot * marginal_usd_lot
                            - marginal_coin_lot * bid_usd_lot;

                        for remaining_usd in 1u128..=16 {
                            let max_usd = core::cmp::min(remaining_usd, bid_usd);
                            if determinant * max_usd / bid_usd_lot > MAX_EXCESS_RESIDUES {
                                continue;
                            }

                            let actual = max_executable_integer_pair(
                                remaining_usd,
                                marginal_coin,
                                marginal_usd,
                                bid_coin,
                                bid_usd,
                                bid_gcd,
                                marginal_gcd,
                            )
                            .unwrap();
                            let mut expected = None;
                            for coin in 1..=bid_coin {
                                let usd = coin * marginal_usd / marginal_coin;
                                if usd == 0
                                    || usd > max_usd
                                    || cmp_bid(coin, usd, bid_coin, bid_usd)
                                        == core::cmp::Ordering::Greater
                                {
                                    continue;
                                }
                                consider_larger_pair(&mut expected, Some((coin, usd)));
                            }
                            assert_eq!(
                                actual,
                                expected,
                                "marginal={marginal_coin}/{marginal_usd}, bid={bid_coin}/{bid_usd}, remaining={remaining_usd}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn integer_pair_solver_matches_exhaustive_oracle_for_all_small_inputs() {
        for marginal_coin in 1u128..=16 {
            for marginal_usd in 1u128..=16 {
                let marginal_gcd = gcd_u64(marginal_coin as u64, marginal_usd as u64);
                for bid_coin in 1u128..=16 {
                    for bid_usd in 1u128..=16 {
                        if cmp_bid(marginal_coin, marginal_usd, bid_coin, bid_usd)
                            == core::cmp::Ordering::Greater
                        {
                            continue;
                        }
                        let bid_gcd = gcd_u64(bid_coin as u64, bid_usd as u64);
                        for remaining_usd in 1u128..=32 {
                            let actual = max_executable_integer_pair(
                                remaining_usd,
                                marginal_coin,
                                marginal_usd,
                                bid_coin,
                                bid_usd,
                                bid_gcd,
                                marginal_gcd,
                            )
                            .unwrap();
                            let max_usd = core::cmp::min(remaining_usd, bid_usd);
                            let mut expected = None;
                            for coin in 1..=bid_coin {
                                let usd = coin * marginal_usd / marginal_coin;
                                if usd == 0
                                    || usd > max_usd
                                    || cmp_bid(coin, usd, bid_coin, bid_usd)
                                        == core::cmp::Ordering::Greater
                                {
                                    continue;
                                }
                                consider_larger_pair(&mut expected, Some((coin, usd)));
                            }
                            assert_eq!(
                                actual, expected,
                                "marginal={marginal_coin}/{marginal_usd}, bid={bid_coin}/{bid_usd}, remaining={remaining_usd}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn wide_product_is_exact_at_u128_boundaries() {
        assert_eq!(wide_product(u128::MAX, u128::MAX), (u128::MAX - 1, 1));
        assert_eq!(wide_product(u128::MAX, 2), (1, u128::MAX - 1));
        assert_eq!(wide_product(1u128 << 127, 1u128 << 127), (1u128 << 126, 0));
        assert_eq!(wide_product(u128::MAX, 1), (0, u128::MAX));
    }

    #[test]
    fn fixed_work_rate_compare_matches_exact_reference() {
        use core::cmp::Ordering;

        fn reference(mut an: u128, mut ad: u128, mut bn: u128, mut bd: u128) -> Ordering {
            let mut reversed = false;
            loop {
                let aq = an / ad;
                let bq = bn / bd;
                if aq != bq {
                    let ordering = aq.cmp(&bq);
                    return if reversed {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                }
                let ar = an % ad;
                let br = bn % bd;
                match (ar == 0, br == 0) {
                    (true, true) => return Ordering::Equal,
                    (true, false) => {
                        return if reversed {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    (false, true) => {
                        return if reversed {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (false, false) => {
                        an = ad;
                        ad = ar;
                        bn = bd;
                        bd = br;
                        reversed = !reversed;
                    }
                }
            }
        }

        let mut state = u128::MAX - 58;
        for _ in 0..2_048 {
            let an = state;
            state ^= state << 23;
            state ^= state >> 17;
            state ^= state << 26;
            let ad = state | 1;
            state = state.wrapping_mul(0xda94_2042_e4dd_58b5_da94_2042_e4dd_58b5);
            let bn = state;
            state ^= state << 19;
            state ^= state >> 29;
            let bd = state | 1;
            assert_eq!(cmp_rate(an, ad, bn, bd), reference(an, ad, bn, bd));
        }
    }

    #[test]
    fn book_layout_fields_dont_overlap() {
        // The slot fields pack tightly and the last one fits inside SLOT_SIZE.
        assert_eq!(SL_COIN_REFUND + 16, SL_PLACE_SLOT);
        assert_eq!(SL_PLACE_SLOT + 8, SL_PLACE_ROUND_END);
        assert_eq!(SL_PLACE_ROUND_END + 8, SLOT_SIZE);
        assert_eq!(BK_ESCROW_BUMP + 1, BK_HOLDING);
        assert_eq!(BK_HOLDING + 32, BK_BID_FEE);
        assert_eq!(BK_BID_FEE + 8, BK_SINK_CUTOFF_SLOT);
        assert_eq!(BK_SINK_CUTOFF_SLOT + 8, BOOK_HEADER);
        assert_eq!(BOOK_SIZE, BOOK_HEADER + MAX_BIDS * SLOT_SIZE);
        assert_eq!(CURRENT_BOOK_LAYOUT.size(), BOOK_SIZE);
        assert_eq!(LEGACY_PRE_CUTOFF_BOOK_LAYOUT.size() + 8, BOOK_SIZE);
        assert_eq!(LEGACY_BID_FEE_BOOK_LAYOUT.header, BK_SINK_CUTOFF_SLOT);
        assert_eq!(LEGACY_HOLDING_BOOK_LAYOUT.header, BK_BID_FEE);
        assert_eq!(INITIAL_BOOK_LAYOUT.header, BK_HOLDING);
        assert_eq!(LEGACY_BID_FEE_BOOK_LAYOUT.slot_size, SL_PLACE_SLOT);
        assert_eq!(LEGACY_HOLDING_BOOK_LAYOUT.slot_size, SL_PLACE_SLOT);
        assert_eq!(INITIAL_BOOK_LAYOUT.slot_size, SL_PLACE_SLOT);
    }

    #[test]
    fn historical_config_and_book_layouts_are_exit_only_and_exact() {
        let mut initial_config = vec![0u8; INITIAL_CONFIG_SIZE];
        initial_config[..8].copy_from_slice(&CONFIG_DISC);
        assert!(validate_exit_config(&initial_config).is_ok());
        assert!(Config::deserialize(&initial_config).is_err());
        initial_config[0] ^= 1;
        assert!(validate_exit_config(&initial_config).is_err());

        for layout in [
            INITIAL_BOOK_LAYOUT,
            LEGACY_HOLDING_BOOK_LAYOUT,
            LEGACY_BID_FEE_BOOK_LAYOUT,
            LEGACY_PRE_CUTOFF_BOOK_LAYOUT,
            CURRENT_BOOK_LAYOUT,
        ] {
            let mut data = vec![0u8; layout.size()];
            data[..8].copy_from_slice(&BOOK_DISC);
            let (_, decoded) = load_exit_book_header(&data).unwrap();
            assert_eq!(decoded.header, layout.header);
            assert_eq!(decoded.slot_size, layout.slot_size);
        }

        for unknown_size in [
            INITIAL_BOOK_LAYOUT.size() + 1,
            LEGACY_HOLDING_BOOK_LAYOUT.size() + 1,
            LEGACY_BID_FEE_BOOK_LAYOUT.size() + 1,
            LEGACY_PRE_CUTOFF_BOOK_LAYOUT.size() + 1,
            BOOK_SIZE - 1,
        ] {
            let mut data = vec![0u8; unknown_size];
            data[..8].copy_from_slice(&BOOK_DISC);
            assert!(load_exit_book_header(&data).is_err());
        }
    }

    #[test]
    fn config_round_trips() {
        let mut c = Config {
            coin_mint: Pubkey::new_unique(),
            market_slab: Pubkey::new_unique(),
            percolator_program: Pubkey::new_unique(),
            squads_multisig: Pubkey::new_unique(),
            metadao_futarchy: Pubkey::new_unique(),
            surplus_buy_burn_bps: DEFAULT_SURPLUS_BUY_BURN_BPS,
            market_0_domain: 0,
            config_bump: 254,
            authority_bump: 251,
            reserved_floor: 123_456_789,
            base_unit_savings_bps: 1_500,
            buyback_bps: 2_000,
            base_unit_savings_account: Pubkey::new_unique(),
            custody_pool: Pubkey::new_unique(),
            custody_principal: 987_654_321,
            custody_mode: CUSTODY_MODE_POOL_BOUND,
            rehandoff_pending: true,
            buyback_remainder_bps: 9_999,
            external_surplus_remainder_bps: 8_888,
            auction_split_remainder_bps: 7_777,
            custody_insurance_spent: 123_456_789_012_345,
        };
        let mut buf = [0u8; CONFIG_SIZE];
        c.serialize(&mut buf).unwrap();
        let d = Config::deserialize(&buf).unwrap();
        assert_eq!(d.coin_mint, c.coin_mint);
        assert_eq!(d.market_slab, c.market_slab);
        assert_eq!(d.squads_multisig, c.squads_multisig);
        assert_eq!(d.metadao_futarchy, c.metadao_futarchy);
        assert_eq!(d.surplus_buy_burn_bps, 8_000);
        assert_eq!(d.authority_bump, 251);
        assert_eq!(d.reserved_floor, 123_456_789);
        assert!(d.surplus_buy_burn_bps < BPS_DENOMINATOR);
        assert_eq!(d.base_unit_savings_bps, 1_500);
        assert_eq!(d.buyback_bps, 2_000);
        assert_eq!(d.base_unit_savings_account, c.base_unit_savings_account);
        assert_eq!(d.custody_pool, c.custody_pool);
        assert_eq!(d.custody_principal, c.custody_principal);
        assert_eq!(d.custody_mode, c.custody_mode);
        assert!(d.rehandoff_pending);
        assert_eq!(d.buyback_remainder_bps, 9_999);
        assert_eq!(d.external_surplus_remainder_bps, 8_888);
        assert_eq!(d.auction_split_remainder_bps, 7_777);
        assert_eq!(d.custody_insurance_spent, c.custody_insurance_spent);

        assert!(c
            .serialize(&mut [0u8; PROVENANCE_CONFIG_SIZE])
            .is_err());
        c.custody_insurance_spent = 0;

        let mut provenance_predecessor = [0u8; PROVENANCE_CONFIG_SIZE];
        c.serialize(&mut provenance_predecessor).unwrap();
        let provenance_old = Config::deserialize(&provenance_predecessor).unwrap();
        assert_eq!(provenance_old.custody_principal, c.custody_principal);
        assert_eq!(provenance_old.custody_mode, c.custody_mode);
        assert_eq!(provenance_old.custody_insurance_spent, 0);

        let mut predecessor = [0u8; CUSTODY_CONFIG_SIZE];
        c.serialize(&mut predecessor).unwrap();
        let old = Config::deserialize(&predecessor).unwrap();
        assert_eq!(old.custody_pool, c.custody_pool);
        assert_eq!(old.custody_principal, 0);
        assert_eq!(old.custody_mode, CUSTODY_MODE_UNATTESTED);
        assert!(!old.rehandoff_pending);
        assert_eq!(old.buyback_remainder_bps, 9_999);
        assert_eq!(old.external_surplus_remainder_bps, 8_888);
        assert_eq!(old.auction_split_remainder_bps, 7_777);

        let mut legacy = [0u8; LEGACY_CONFIG_SIZE];
        c.serialize(&mut legacy).unwrap();
        let legacy_config = Config::deserialize(&legacy).unwrap();
        assert_eq!(legacy_config.buyback_remainder_bps, 9_999);
        assert_eq!(legacy_config.external_surplus_remainder_bps, 8_888);
        assert_eq!(legacy_config.auction_split_remainder_bps, 7_777);

        // The immediate predecessor wrote a standalone little-endian buyback u16 at the start
        // of this reserved range. It remains a valid packed value with both new carries zero.
        buf[267..272].fill(0);
        buf[267..269].copy_from_slice(&9_999u16.to_le_bytes());
        let predecessor_buyback_only = Config::deserialize(&buf).unwrap();
        assert_eq!(predecessor_buyback_only.buyback_remainder_bps, 9_999);
        assert_eq!(predecessor_buyback_only.external_surplus_remainder_bps, 0);
        assert_eq!(predecessor_buyback_only.auction_split_remainder_bps, 0);

        buf[265] = CUSTODY_MODE_POOLLESS_EMPTY + 1;
        assert!(matches!(
            Config::deserialize(&buf),
            Err(ProgramError::InvalidAccountData)
        ));
        buf[265] = CUSTODY_MODE_POOL_BOUND;
        buf[266] = 2;
        assert!(matches!(
            Config::deserialize(&buf),
            Err(ProgramError::InvalidAccountData)
        ));
        buf[266] = 1;
        let invalid_packed = 1_000_000_000_000u64.to_le_bytes();
        buf[267..272].copy_from_slice(&invalid_packed[..5]);
        assert!(matches!(
            Config::deserialize(&buf),
            Err(ProgramError::InvalidAccountData)
        ));
    }
}
