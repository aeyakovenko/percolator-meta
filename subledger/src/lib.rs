//! Asset-local insurance / backing subledger.
//!
//! A reusable, **owner-bound** deposit pool that permissionless asset programs
//! (Percolator markets/assets 1..N) can use to offer local insurance/backing
//! deposits that earn local fees/yield. It is deliberately *not* part of genesis
//! COIN farming and the MetaDAO has **no authority over it** — there is no admin,
//! no governance key, no upgrade-of-policy path. Each depositor can exit their own
//! position. After genesis finalization and terminal market resolution, a public
//! cranker can only return the complete position to a clean account owned by that
//! depositor; nobody can redirect it.
//!
//! Accounting (per pool):
//!   - `outstanding_principal` = sum of un-withdrawn deposit principal.
//!   - `asset_balance`         = the pool vault's live token balance (principal +
//!     any fees/yield transferred in, minus impairment).
//!
//! Exit policy:
//!   - `Principal`    - pay at most principal, with impairment allocated by shares
//!     priced against `min(balance, outstanding)`. Surplus stays in the pool. Historical
//!     pre-share positions retain their pool-wide pro-rata exit.
//!   - `WithSurplus`  - redeem live-priced shares, so local fees/yield are returned
//!     to depositors according to their entry price.

#![no_std]
extern crate alloc;

#[allow(unused_imports)]
use alloc::format; // required by the entrypoint!/msg! macro in SBF builds
use alloc::{vec, vec::Vec};
use percolator_accounting::InsuranceWithdrawalPlan;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    declare_id,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
    sysvar::Sysvar,
};

declare_id!("Sub1edger1111111111111111111111111111111111");

const POOL_DISC: [u8; 8] = *b"SUBPOOL1";
const POSITION_DISC: [u8; 8] = *b"SUBPOS01";
// Pool now also carries the Percolator refs (market_slab + percolator_program) so
// an insurance pool can sign domain top-ups / asset-wide withdrawals as the asset-0
// insurance authority/operator. Own-vault pools leave them zero. The trailing
// vote_authority (the genesis-vote config PDA) may toggle a position's vote-lock.
// Branch `risidual_genesis_never_push_upstream`: POLICY_WITH_SURPLUS pools are now
// SHARE-based so exit pays a TENURE-FAIR slice of the surplus (a late depositor cannot
// claim surplus that accrued before it joined — and cannot extract early backers' surplus
// on exit, the soft-veto fairness prerequisite). Pool grows by `total_shares` (u128 @192)
// and `coin_mint` (Pubkey @208) so the genesis-vote authority is part of the pool namespace.
// Position grows by `shares` (u128 @104). Reserved bytes hold a pool share
// generation (u40 @91) and an active-position generation (u40 @99); terminal
// positions continue to use @99 for their return slot. All cross-program reads (genesis-vote
// principal@72 / start_slot@89 / outstanding@80) keep their offsets — the new fields are
// appended, so those programs are unaffected. Byte 272 is a flags byte for
// cross-backing and the immutable first-custody-grant commitment. Current
// genesis pools append the slot of that first grant so no transaction can
// atomically grant custody and admit a replayable deposit.
// Historical accounts cannot be reallocated by an upgrade. Keep every deployed
// wire size readable so owners retain an exit path, while new pools always use
// the current layout. Sizes that never existed remain invalid.
const POOL_SIZE_BASE: usize = 96;
const POOL_SIZE_MARKET: usize = 160;
const POOL_SIZE_VOTE: usize = 192;
const POOL_SIZE_SHARES: usize = 208;
const POOL_SIZE_DEADLINE_ONLY: usize = 216;
const POOL_SIZE_COIN: usize = 240;
const POOL_SIZE_WINDOW: usize = 256;
const POOL_SIZE_START: usize = 264;
const POOL_SIZE: usize = 272;
const POOL_SIZE_CROSS_BACKING_V1: usize = 273;
const POOL_SIZE_CROSS_BACKING_V2: usize = 289;
const POOL_SIZE_CROSS_BACKING_V3: usize = 321;
const POOL_SIZE_CUSTODY_GRANT_LEGACY: usize = 329;
const POOL_SIZE_CROSS_BACKING: usize = 345;
const POOL_SIZE_CUSTODY_GRANT: usize = 361;
const POOL_FLAGS_OFF: usize = 272;
const POOL_SIZE_CUSTODY_FLAGS: usize = POOL_SIZE_CROSS_BACKING_V1;
const POOL_FLAG_CROSS_BACKING: u8 = 1 << 0;
const POOL_FLAG_CUSTODY_GRANTED: u8 = 1 << 1;
const POOL_FLAGS_MASK: u8 = POOL_FLAG_CROSS_BACKING | POOL_FLAG_CUSTODY_GRANTED;
const POOL_PENDING_BACKING_OFF: usize = 273;
const POOL_SHARE_RATE_NUMERATOR_OFF: usize = 289;
const POOL_SHARE_RATE_DENOMINATOR_OFF: usize = 305;
const POOL_INSURANCE_SPENT_CHECKPOINT_OFF: usize = 321;
const POOL_BACKING_PROTECTED_CHECKPOINT_OFF: usize = 337;
// Bytes 345..353 are reserved for the ordinary-principal loss checkpoint.
const POOL_CUSTODY_GRANT_SLOT_OFF: usize = 353;
const POOL_CUSTODY_GRANT_SLOT_LEGACY_OFF: usize = 321;
const POSITION_SIZE_BASE: usize = 96;
const POSITION_SIZE_TENURE: usize = 104;
const POSITION_SIZE: usize = 120;
// One week at ~400ms/slot. `init_insurance_pool` accepts an optional explicit
// slot window, but defaults to this short bootstrap deposit window.
const DEFAULT_GENESIS_DEPOSIT_WINDOW_SLOTS: u64 = 1_512_000;
// Default genesis pools open immediately in tests/local deployments. Production
// genesis should pass an explicit future start slot.
const DEFAULT_GENESIS_DEPOSIT_START_SLOT: u64 = 0;
// Six months at ~400ms/slot. Keep this in lockstep with genesis-vote's default.
const DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS: u64 = 38_880_000;
const OWN_VAULT_DEPOSIT_WINDOW_SLOTS: u64 = u64::MAX;
const OWN_VAULT_DEPOSIT_START_SLOT: u64 = 0;
const OWN_VAULT_BOOTSTRAP_DELAY_SLOTS: u64 = 0;

// Position field byte offsets, exposed so cross-program readers (genesis-vote, residual-distributor)
// can PIN their hardcoded reads against this canonical layout instead of guessing (finding HF: a
// consumer's wrong owner offset slipped past mocked tests). Authoritative: the `position_layout`
// canary below asserts these match `Position::serialize`.
pub const POS_POOL_OFF: usize = 8;
pub const POS_OWNER_OFF: usize = 40;
pub const POS_PRINCIPAL_OFF: usize = 72;
// Active positions use this field as an action nonce. A permissionless terminal
// return overwrites it with the principal-at-risk reward snapshot.
pub const POS_ACTION_NONCE_OFF: usize = 80;
pub const POS_WITHDRAWN_AMOUNT_OFF: usize = 80;
pub const POS_WITHDRAWN_OFF: usize = 88;
pub const POS_START_SLOT_OFF: usize = 89;
pub const POS_TERMINAL_RETURNED_OFF: usize = 98;
pub const POS_TERMINAL_RETURN_SLOT_OFF: usize = 99;
// Active positions use the terminal-slot bytes for their share generation. Once
// terminal_returned is set, those same bytes retain their documented timestamp.
pub const POS_SHARE_GENERATION_OFF: usize = POS_TERMINAL_RETURN_SLOT_OFF;
pub const POS_SHARES_OFF: usize = 104; // Position.shares (POLICY_WITH_SURPLUS) — the share-value points source.
                                       // Pool.outstanding_principal — the quorum denominator the genesis-vote reads (finding ID). Exported
                                       // + canaried so a consumer's mirror offset can be cross-pinned, same discipline as the POS_* offsets.
pub const POOL_OUTSTANDING_PRINCIPAL_OFF: usize = 80;
pub const POOL_SHARE_GENERATION_OFF: usize = 91;
pub const POOL_DEPOSIT_DEADLINE_SLOT_OFF: usize = 240;
pub const POOL_DEPOSIT_WINDOW_SLOTS_OFF: usize = 248;
pub const POOL_DEPOSIT_START_SLOT_OFF: usize = 256;
pub const POOL_BOOTSTRAP_DELAY_SLOTS_OFF: usize = 264;

const POLICY_PRINCIPAL: u8 = 0;
const POLICY_WITH_SURPLUS: u8 = 1;

// Which Percolator domain this pool backs. asset-0 insurance is the principal-only
// vote bond; backing (asset 0) and assets 1..N run with-surplus.
const DOMAIN_INSURANCE: u8 = 0;
const DOMAIN_BACKING: u8 = 1;

const U40_MAX: u64 = (1u64 << 40) - 1;
const MAX_TERMINAL_RETURN_SLOT: u64 = U40_MAX - 1;
// The existing u40 field carries independent 20-bit counters: exact-loss resets in the low half
// and lazy ratio-preserving rescalings in the high half. No account layout or authority changes.
const SHARE_GENERATION_BITS: u32 = 20;
const SHARE_GENERATION_MASK: u64 = (1u64 << SHARE_GENERATION_BITS) - 1;
const VIRTUAL_SHARE_SCALE_LIMIT: u64 = 20;

fn decode_u40(bytes: &[u8]) -> u64 {
    let mut encoded = [0u8; 8];
    encoded[..5].copy_from_slice(bytes);
    u64::from_le_bytes(encoded)
}

fn encode_u40(value: u64, bytes: &mut [u8]) -> ProgramResult {
    if value > U40_MAX || bytes.len() != 5 {
        return Err(ProgramError::InvalidAccountData);
    }
    bytes.copy_from_slice(&value.to_le_bytes()[..5]);
    Ok(())
}

// The SPL Associated Token Account program. Percolator pins each market vault to
// the single CANONICAL ATA of (vault_authority, mint) — its finding F-VAULT-FRAG.
// We mirror that derivation so a pool can only ever bind to the exact vault
// Percolator will accept, failing fast at init instead of dead on first deposit.
const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const GENESIS_VOTE_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("GenesisVote11111111111111111111111111111111");
const GENESIS_IX_RETIRE_TERMINAL_BALLOT: u8 = 7;
const MARKET_CONTROLLER_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("3ueoyr1JepT2DvPxh8LrhdJZ6YsL2sT9Sm7y3TfNyfi9");

fn canonical_vault_address(vault_authority: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            vault_authority.as_ref(),
            spl_token::ID.as_ref(),
            mint.as_ref(),
        ],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

const IX_INIT_POOL: u8 = 0;
const IX_DEPOSIT: u8 = 1;
const IX_WITHDRAW: u8 = 2;
const IX_INIT_INSURANCE_POOL: u8 = 3;
const IX_INSURANCE_DEPOSIT: u8 = 4;
const IX_INSURANCE_WITHDRAW: u8 = 5;
// Toggle a position's vote-lock. Callable ONLY by the pool's registered
// vote_authority (the genesis-vote config PDA). While locked, the owner cannot
// insurance-withdraw — they must retract their genesis vote first, which clears
// the lock. This binds the vote's principal snapshot to capital that is still at
// risk (closes the vote-outlives-capital vector).
const IX_SET_VOTE_LOCK: u8 = 6;
// Atomically receive the asset-0 insurance authority, operator, and asset-admin roles.
// Taking asset_admin away from the governance vault is what prevents governance from
// later reassigning the operator to itself and bypassing owner-bound withdrawals.
const IX_ACCEPT_OPERATOR: u8 = 7;
// Governance-authorized, hardcoded pool -> TWAP custody handoff. Principal pools carry
// their protected floor; with-surplus pools are accepted only after all owner claims exit.
// The pool remains current asset_admin until TWAP atomically receives all three roles.
const IX_HANDOFF_TO_TWAP: u8 = 8;
// Permissionless resolved-mode backing return. The pool signs only the controller's
// fixed, recipient-free asset-0 cleanup after custody has returned from TWAP.
const IX_RETURN_RESOLVED_ASSET0_BACKING: u8 = 9;
// Read-only CPI attestation for TWAP's terminal protocol-insurance recovery.
const IX_ASSERT_NO_PRINCIPAL: u8 = 10;
// Read-only CPI attestation for a permissionless resolved custody return. Keeping
// this check in the subledger supports every pool layout without duplicating it in TWAP.
const IX_ASSERT_PRINCIPAL: u8 = 11;
// Permissionless terminal return for an absent genesis depositor. Genesis admits
// it immediately after a seal or after the unsealed trigger phase expires. It
// retires the complete position and can pay only a clean token account owned by
// that depositor.
const IX_RETURN_FINALIZED_POSITION: u8 = 12;
// Owner-signed full exit, committed to the position's principal, last deposit
// slot, and action nonce. TWAP forwards the same signed witness after returning
// established custody.
const IX_INSURANCE_WITHDRAW_FULL: u8 = 13;
// New genesis pools split aggregate principal equally between asset-0 insurance
// and asset-0 market-risk backing. The one-byte layout discriminator keeps every
// historical insurance-only pool readable without silently changing its exits.
const IX_INIT_CROSS_BACKING_GENESIS_POOL: u8 = 14;
// Permissionless, amountless routing of cross-backing utilization earnings.
// The pool reads both exact Percolator counters and TWAP fixes the recipient to
// the config-bound Squads vault; no caller controls an amount or owner.
const IX_ROUTE_CROSS_BACKING_EARNINGS: u8 = 15;

// Percolator CPI tags (verified against pinned percolator-prog 19f3b494).
const PERC_IX_TOP_UP_INSURANCE_DOMAIN: u8 = 56;
const PERC_IX_TOP_UP_BACKING_BUCKET: u8 = 24;
const PERC_IX_WITHDRAW_BACKING_BUCKET: u8 = 50;
const PERC_IX_WITHDRAW_BACKING_BUCKET_EARNINGS: u8 = 52;
const PERC_IX_SYNC_BACKING_DOMAIN_LEDGER: u8 = 53;
// tag 57 = WithdrawInsuranceAsset { asset_index: u16, amount: u128 } — the consolidated, asset-indexed,
// insurance-operator-gated, during-Live insurance withdraw that replaced the removed asset-0 tag-23.
// The percolator caps `amount` to the available
// insurance; the subledger's own per-owner owed computation is the depositor-principal cap on top.
const PERC_IX_WITHDRAW_INSURANCE_ASSET: u8 = 57;
const PERC_IX_UPDATE_ASSET_AUTHORITY: u8 = 65;
const ASSET_AUTH_ADMIN: u8 = 0;
const ASSET_AUTH_INSURANCE: u8 = 1; // insurance_authority (gates insurance top-ups)
const ASSET_AUTH_INSURANCE_OPERATOR: u8 = 2; // insurance_operator (gates insurance withdrawals)
const ASSET_AUTH_BACKING_BUCKET: u8 = 3;
const BACKING_LEDGER_SEED: &[u8] = b"subledger_backing_ledger";
const CROSS_BACKING_POOL_SEED: &[u8] = b"cross-backing";
const TWAP_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TwapBuyBurn11111111111111111111111111111111");
const TWAP_AUTHORITY_SEED: &[u8] = b"market-0-twap";
const TWAP_CONFIG_DISC: [u8; 8] = *b"TWAPCFG1";
const TWAP_CUSTODY_CONFIG_MIN_SIZE: usize = 264;
const TWAP_PROVENANCE_CONFIG_MIN_SIZE: usize = 272;
const TWAP_LOSS_CHECKPOINT_CONFIG_MIN_SIZE: usize = 288;
const TWAP_CUSTODY_PRINCIPAL_OFF: usize = 257;
const TWAP_CUSTODY_MODE_OFF: usize = 265;
const TWAP_INSURANCE_SPENT_OFF: usize = 272;
const TWAP_CUSTODY_MODE_POOL_BOUND: u8 = 1;
const TWAP_IX_ACCEPT_FROM_SUBLEDGER: u8 = 15;
const TWAP_IX_ACCEPT_CROSS_BACKING_EARNINGS: u8 = 22;
const CONTROLLER_IX_RETURN_RESOLVED_ASSET0_BACKING: u8 = 7;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

// The pool PDA commits to its market binding, distributed COIN mint, policy, and domain, not just
// (collateral mint, asset_id). Keying it on (mint, asset_id) alone made init_insurance_pool
// (permissionless) front-run squattable:
// the genesis pool PDA = f(COIN_mint, 0) and the gv config PDA = f(COIN_mint) are both
// predictable, so an attacker could init the pool FIRST bound to a percolator market
// THEY control (passing that market's canonical insurance vault) with vote_authority set
// to the predictable real gv config PDA — satisfying the gv binding check. Genesis would
// then wire to a pool that routes every depositor's principal into the attacker's market
// (TopUpInsurance), where the attacker (its marketauth) can strand or bleed it: LOF, not
// just DOS. Folding market_slab + percolator_program into the seed means the only pool
// that can exist at the legit address is bound to the legit market. Folding coin_mint,
// deposit_window_slots, deposit_start_slot, and bootstrap_delay_slots into the seed closes
// same-market squats: a
// hostile init using a fake COIN mint, fake genesis-vote authority, wrong deposit window, or
// early schedule lands at a different PDA instead of consuming the real one.
// Own-vault pools use Pubkey::default() for the market/program/coin slots, matching what
// they store. Policy/domain are seed material because a principal-only first-writer could
// otherwise consume the intended share-based pool PDA and disable soft-veto rewards while
// still passing all market/vault/authority checks. (finding Q; same class as finding P.)
fn pool_seeds_full<'a>(
    mint: &'a Pubkey,
    asset_id: &'a [u8; 8],
    market_slab: &'a Pubkey,
    percolator_program: &'a Pubkey,
    coin_mint: &'a Pubkey,
    policy: &'a [u8; 1],
    domain: &'a [u8; 1],
    deposit_window_slots: &'a [u8; 8],
    deposit_start_slot: &'a [u8; 8],
    bootstrap_delay_slots: &'a [u8; 8],
) -> [&'a [u8]; 11] {
    [
        b"subledger_pool",
        mint.as_ref(),
        asset_id,
        market_slab.as_ref(),
        percolator_program.as_ref(),
        coin_mint.as_ref(),
        policy,
        domain,
        deposit_window_slots,
        deposit_start_slot,
        bootstrap_delay_slots,
    ]
}

fn position_seeds<'a>(pool: &'a Pubkey, owner: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"subledger_position", pool.as_ref(), owner.as_ref()]
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Pool {
    mint: Pubkey,
    /// Percolator asset index this pool attributes (0 = market-0).
    asset_id: u64,
    /// The token account principal flows through. For own-vault pools this is the
    /// pool-PDA-owned SPL account; for insurance pools it is the Percolator
    /// market's canonical insurance vault (the ATA of its vault_authority).
    vault: Pubkey,
    /// `outstanding_principal` is the quorum denominator the genesis-vote reads:
    /// the sum of live (un-withdrawn) deposit principal in this pool.
    outstanding_principal: u64,
    policy: u8,
    domain: u8, // DOMAIN_INSURANCE | DOMAIN_BACKING
    bump: u8,
    /// Monotonic generation for priced shares. A fully impaired insurance pool
    /// advances this before accepting recapitalization, making prior shares
    /// permanently valueless without iterating depositor accounts.
    share_generation: u64,
    /// Percolator market slab this insurance pool tops up / withdraws from.
    /// `Pubkey::default()` for own-vault pools.
    market_slab: Pubkey,
    /// Percolator program id. `Pubkey::default()` for own-vault pools.
    percolator_program: Pubkey,
    /// Authority allowed to toggle a position's vote-lock (the genesis-vote config
    /// PDA). `Pubkey::default()` disables vote-locking (own-vault pools).
    vote_authority: Pubkey,
    /// Total pricing shares. Legacy layouts price deposits and withdrawals from
    /// the live balance and leave floor remainders as unowned reserve shares.
    /// Current cross-backed pools burn the owner's exact shares against the
    /// loss-only rational rate below, so exit order cannot move those remainders
    /// into another owner's claim.
    total_shares: u128,
    /// Distributed COIN mint whose genesis-vote config may lock this pool's positions.
    /// Own-vault pools store Pubkey::default().
    coin_mint: Pubkey,
    /// First slot at which new deposits are rejected. Own-vault pools set this to
    /// u64::MAX; genesis insurance pools set a short bootstrap deposit deadline.
    deposit_deadline_slot: u64,
    /// Configured window committed into the pool PDA. Own-vault pools use the
    /// no-close sentinel.
    deposit_window_slots: u64,
    /// First slot at which deposits are allowed. Bound into the PDA so a
    /// permissionless first writer cannot start the window early.
    deposit_start_slot: u64,
    /// Delay from `deposit_start_slot` until genesis may be triggered. Insurance
    /// pools bind this to the genesis-vote config; own-vault pools use zero.
    bootstrap_delay_slots: u64,
    /// Current genesis pools also custody the asset-0 backing role and split each
    /// aggregate deposit across insurance and backing. Historical layouts leave
    /// this false and retain their insurance-only behavior.
    cross_backing: bool,
    /// A fresh Percolator custody grant is one-shot for this pool incarnation.
    /// Raw slab reuse must create a differently seeded pool rather than revive
    /// signatures that users authorized for the original market.
    custody_granted: bool,
    /// First successful custody-grant slot encoded as slot + 1. Zero means no
    /// grant. Deposits must execute in a later slot, so grant-plus-deposit
    /// transactions cannot become valid signatures for a later slab generation.
    custody_grant_slot_plus_one: u64,
    /// Genesis backing that could not enter a Percolator domain because public
    /// trader-source capital fixed a conflicting bucket expiry. These atoms stay
    /// in the canonical pool ATA and remain owner principal, segregated by domain.
    pending_backing: [u64; 2],
    /// Exact owner-claim value per share for current cross-backed principal
    /// pools. External protection losses may lower this rational accumulator;
    /// deposits and exits never change it, so one position's floor remainder
    /// cannot move another position's claim.
    share_rate_numerator: u128,
    share_rate_denominator: u128,
    /// Monotonic Percolator insurance consumption already applied to the
    /// indexed owner claim.
    insurance_spent_checkpoint: u128,
    /// Canonical owner backing observed when the indexed claim was last priced.
    /// This excludes trader source backing, utilization earnings, and backing
    /// above the pool's nominal 50/50 tranche.
    backing_protected_checkpoint: u64,
}

impl Pool {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if !supported_pool_size(data.len()) || data[..8] != POOL_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let policy = data[88];
        let domain = data[90];
        let pool_flags = if data.len() >= POOL_SIZE_CUSTODY_FLAGS {
            data[POOL_FLAGS_OFF]
        } else {
            0
        };
        if pool_flags & !POOL_FLAGS_MASK != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        let cross_backing = pool_flags & POOL_FLAG_CROSS_BACKING != 0;
        let custody_granted = pool_flags & POOL_FLAG_CUSTODY_GRANTED != 0;
        let custody_grant_slot_plus_one =
            if let Some(offset) = custody_grant_slot_offset(data.len()) {
                u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
            } else {
                0
            };
        if policy > POLICY_WITH_SURPLUS
            || domain > DOMAIN_BACKING
            || (cross_backing && (policy != POLICY_PRINCIPAL || domain != DOMAIN_INSURANCE))
            || custody_granted != (custody_grant_slot_plus_one != 0)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            asset_id: u64::from_le_bytes(data[40..48].try_into().unwrap()),
            vault: Pubkey::new_from_array(data[48..80].try_into().unwrap()),
            outstanding_principal: u64::from_le_bytes(data[80..88].try_into().unwrap()),
            policy,
            domain,
            bump: data[89],
            share_generation: if data.len() >= POOL_SIZE_SHARES {
                decode_u40(&data[POOL_SHARE_GENERATION_OFF..POOL_SHARE_GENERATION_OFF + 5])
            } else {
                0
            },
            market_slab: if data.len() >= POOL_SIZE_MARKET {
                Pubkey::new_from_array(data[96..128].try_into().unwrap())
            } else {
                Pubkey::default()
            },
            percolator_program: if data.len() >= POOL_SIZE_MARKET {
                Pubkey::new_from_array(data[128..160].try_into().unwrap())
            } else {
                Pubkey::default()
            },
            vote_authority: if data.len() >= POOL_SIZE_VOTE {
                Pubkey::new_from_array(data[160..192].try_into().unwrap())
            } else {
                Pubkey::default()
            },
            total_shares: if data.len() >= POOL_SIZE_SHARES {
                u128::from_le_bytes(data[192..208].try_into().unwrap())
            } else {
                0
            },
            coin_mint: if data.len() >= POOL_SIZE_COIN {
                Pubkey::new_from_array(data[208..240].try_into().unwrap())
            } else {
                Pubkey::default()
            },
            deposit_deadline_slot: if data.len() == POOL_SIZE_DEADLINE_ONLY {
                u64::from_le_bytes(data[208..216].try_into().unwrap())
            } else if data.len() >= POOL_SIZE_WINDOW {
                u64::from_le_bytes(data[240..248].try_into().unwrap())
            } else {
                u64::MAX
            },
            deposit_window_slots: if data.len() >= POOL_SIZE_WINDOW {
                u64::from_le_bytes(data[248..256].try_into().unwrap())
            } else {
                OWN_VAULT_DEPOSIT_WINDOW_SLOTS
            },
            deposit_start_slot: if data.len() >= POOL_SIZE_START {
                u64::from_le_bytes(data[256..264].try_into().unwrap())
            } else {
                OWN_VAULT_DEPOSIT_START_SLOT
            },
            bootstrap_delay_slots: if data.len() >= POOL_SIZE {
                u64::from_le_bytes(data[264..272].try_into().unwrap())
            } else {
                OWN_VAULT_BOOTSTRAP_DELAY_SLOTS
            },
            cross_backing,
            custody_granted,
            custody_grant_slot_plus_one,
            pending_backing: if data.len() >= POOL_SIZE_CROSS_BACKING_V2 {
                [
                    u64::from_le_bytes(
                        data[POOL_PENDING_BACKING_OFF..POOL_PENDING_BACKING_OFF + 8]
                            .try_into()
                            .unwrap(),
                    ),
                    u64::from_le_bytes(
                        data[POOL_PENDING_BACKING_OFF + 8..POOL_PENDING_BACKING_OFF + 16]
                            .try_into()
                            .unwrap(),
                    ),
                ]
            } else {
                [0, 0]
            },
            share_rate_numerator: if data.len() >= POOL_SIZE_CROSS_BACKING_V3 {
                u128::from_le_bytes(
                    data[POOL_SHARE_RATE_NUMERATOR_OFF..POOL_SHARE_RATE_NUMERATOR_OFF + 16]
                        .try_into()
                        .unwrap(),
                )
            } else {
                0
            },
            share_rate_denominator: if data.len() >= POOL_SIZE_CROSS_BACKING_V3 {
                u128::from_le_bytes(
                    data[POOL_SHARE_RATE_DENOMINATOR_OFF..POOL_SHARE_RATE_DENOMINATOR_OFF + 16]
                        .try_into()
                        .unwrap(),
                )
            } else {
                0
            },
            insurance_spent_checkpoint: if data.len() >= POOL_SIZE_CROSS_BACKING {
                u128::from_le_bytes(
                    data[POOL_INSURANCE_SPENT_CHECKPOINT_OFF
                        ..POOL_INSURANCE_SPENT_CHECKPOINT_OFF + 16]
                        .try_into()
                        .unwrap(),
                )
            } else {
                0
            },
            backing_protected_checkpoint: if data.len() >= POOL_SIZE_CROSS_BACKING {
                u64::from_le_bytes(
                    data[POOL_BACKING_PROTECTED_CHECKPOINT_OFF
                        ..POOL_BACKING_PROTECTED_CHECKPOINT_OFF + 8]
                        .try_into()
                        .unwrap(),
                )
            } else {
                0
            },
        })
    }

    fn serialize(&self, data: &mut [u8]) -> ProgramResult {
        if !supported_pool_size(data.len()) {
            return Err(ProgramError::InvalidAccountData);
        }
        data[..8].copy_from_slice(&POOL_DISC);
        data[8..40].copy_from_slice(self.mint.as_ref());
        data[40..48].copy_from_slice(&self.asset_id.to_le_bytes());
        data[48..80].copy_from_slice(self.vault.as_ref());
        data[80..88].copy_from_slice(&self.outstanding_principal.to_le_bytes());
        data[88] = self.policy;
        data[89] = self.bump;
        data[90] = self.domain;
        if data.len() >= POOL_SIZE_SHARES {
            encode_u40(self.share_generation, &mut data[91..96])?;
        } else {
            data[91..96].fill(0);
        }
        if data.len() >= POOL_SIZE_MARKET {
            data[96..128].copy_from_slice(self.market_slab.as_ref());
            data[128..160].copy_from_slice(self.percolator_program.as_ref());
        }
        if data.len() >= POOL_SIZE_VOTE {
            data[160..192].copy_from_slice(self.vote_authority.as_ref());
        }
        if data.len() >= POOL_SIZE_SHARES {
            data[192..208].copy_from_slice(&self.total_shares.to_le_bytes());
        }
        if data.len() == POOL_SIZE_DEADLINE_ONLY {
            data[208..216].copy_from_slice(&self.deposit_deadline_slot.to_le_bytes());
        } else if data.len() >= POOL_SIZE_COIN {
            data[208..240].copy_from_slice(self.coin_mint.as_ref());
        }
        if data.len() >= POOL_SIZE_WINDOW {
            data[240..248].copy_from_slice(&self.deposit_deadline_slot.to_le_bytes());
            data[248..256].copy_from_slice(&self.deposit_window_slots.to_le_bytes());
        }
        if data.len() >= POOL_SIZE_START {
            data[256..264].copy_from_slice(&self.deposit_start_slot.to_le_bytes());
        }
        if data.len() >= POOL_SIZE {
            data[264..272].copy_from_slice(&self.bootstrap_delay_slots.to_le_bytes());
        }
        if data.len() >= POOL_SIZE_CUSTODY_FLAGS {
            data[POOL_FLAGS_OFF] = u8::from(self.cross_backing)
                | (u8::from(self.custody_granted) << 1);
        } else if self.cross_backing || self.custody_granted {
            return Err(ProgramError::InvalidAccountData);
        }
        if data.len() >= POOL_SIZE_CROSS_BACKING_V2 {
            data[POOL_PENDING_BACKING_OFF..POOL_PENDING_BACKING_OFF + 8]
                .copy_from_slice(&self.pending_backing[0].to_le_bytes());
            data[POOL_PENDING_BACKING_OFF + 8..POOL_PENDING_BACKING_OFF + 16]
                .copy_from_slice(&self.pending_backing[1].to_le_bytes());
        } else if self.pending_backing != [0, 0] {
            return Err(ProgramError::InvalidAccountData);
        }
        if data.len() >= POOL_SIZE_CROSS_BACKING_V3 {
            data[POOL_SHARE_RATE_NUMERATOR_OFF..POOL_SHARE_RATE_NUMERATOR_OFF + 16]
                .copy_from_slice(&self.share_rate_numerator.to_le_bytes());
            data[POOL_SHARE_RATE_DENOMINATOR_OFF..POOL_SHARE_RATE_DENOMINATOR_OFF + 16]
                .copy_from_slice(&self.share_rate_denominator.to_le_bytes());
        } else if self.share_rate_numerator != 0 || self.share_rate_denominator != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        if data.len() >= POOL_SIZE_CROSS_BACKING {
            data[POOL_INSURANCE_SPENT_CHECKPOINT_OFF
                ..POOL_INSURANCE_SPENT_CHECKPOINT_OFF + 16]
                .copy_from_slice(&self.insurance_spent_checkpoint.to_le_bytes());
        } else if self.insurance_spent_checkpoint != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        if data.len() >= POOL_SIZE_CROSS_BACKING {
            data[POOL_BACKING_PROTECTED_CHECKPOINT_OFF
                ..POOL_BACKING_PROTECTED_CHECKPOINT_OFF + 8]
                .copy_from_slice(&self.backing_protected_checkpoint.to_le_bytes());
        } else if self.backing_protected_checkpoint != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        if self.custody_granted != (self.custody_grant_slot_plus_one != 0) {
            return Err(ProgramError::InvalidAccountData);
        }
        if let Some(offset) = custody_grant_slot_offset(data.len()) {
            data[offset..offset + 8]
                .copy_from_slice(&self.custody_grant_slot_plus_one.to_le_bytes());
        } else if self.custody_grant_slot_plus_one != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    fn is_insurance(&self) -> bool {
        self.percolator_program != Pubkey::default()
    }

    fn owner_claims_cleared(&self) -> bool {
        self.outstanding_principal == 0
            && self.pending_backing == [0, 0]
            && match self.policy {
                POLICY_PRINCIPAL => self.total_shares == 0,
                // Empty share pools normalize any whole-atom rounding reserve
                // into unowned pricing shares for a possible later deposit epoch.
                POLICY_WITH_SURPLUS => true,
                _ => false,
            }
    }

    fn pending_backing_total(&self) -> Result<u64, ProgramError> {
        self.pending_backing[0]
            .checked_add(self.pending_backing[1])
            .ok_or(ProgramError::InvalidAccountData)
    }
}

fn custody_grant_slot_offset(pool_size: usize) -> Option<usize> {
    if pool_size == POOL_SIZE_CUSTODY_GRANT_LEGACY {
        Some(POOL_CUSTODY_GRANT_SLOT_LEGACY_OFF)
    } else if pool_size >= POOL_SIZE_CUSTODY_GRANT {
        Some(POOL_CUSTODY_GRANT_SLOT_OFF)
    } else {
        None
    }
}

fn supported_pool_size(size: usize) -> bool {
    matches!(
        size,
        POOL_SIZE_BASE
            | POOL_SIZE_MARKET
            | POOL_SIZE_VOTE
            | POOL_SIZE_SHARES
            | POOL_SIZE_DEADLINE_ONLY
            | POOL_SIZE_COIN
            | POOL_SIZE_WINDOW
            | POOL_SIZE_START
    ) || size >= POOL_SIZE
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PoolSeedVersion {
    Base,
    Market,
    Coin,
    PolicyDomain,
    Window,
    Start,
    Bootstrap,
    CrossBacking,
}

struct PoolSeedBytes {
    asset_id: [u8; 8],
    policy: [u8; 1],
    domain: [u8; 1],
    deposit_window_slots: [u8; 8],
    deposit_start_slot: [u8; 8],
    bootstrap_delay_slots: [u8; 8],
    bump: [u8; 1],
}

impl PoolSeedBytes {
    fn new(pool: &Pool) -> Self {
        Self {
            asset_id: pool.asset_id.to_le_bytes(),
            policy: [pool.policy],
            domain: [pool.domain],
            deposit_window_slots: pool.deposit_window_slots.to_le_bytes(),
            deposit_start_slot: pool.deposit_start_slot.to_le_bytes(),
            bootstrap_delay_slots: pool.bootstrap_delay_slots.to_le_bytes(),
            bump: [pool.bump],
        }
    }

    fn signer_seeds<'a>(&'a self, pool: &'a Pool, version: PoolSeedVersion) -> Vec<&'a [u8]> {
        let mut seeds: Vec<&[u8]> = Vec::with_capacity(12);
        seeds.extend_from_slice(&[b"subledger_pool", pool.mint.as_ref(), &self.asset_id]);
        match version {
            PoolSeedVersion::Base => {}
            PoolSeedVersion::Market => seeds
                .extend_from_slice(&[pool.market_slab.as_ref(), pool.percolator_program.as_ref()]),
            PoolSeedVersion::Coin => seeds.extend_from_slice(&[
                pool.market_slab.as_ref(),
                pool.percolator_program.as_ref(),
                pool.coin_mint.as_ref(),
            ]),
            PoolSeedVersion::PolicyDomain => seeds.extend_from_slice(&[
                pool.market_slab.as_ref(),
                pool.percolator_program.as_ref(),
                pool.coin_mint.as_ref(),
                &self.policy,
                &self.domain,
            ]),
            PoolSeedVersion::Window => seeds.extend_from_slice(&[
                pool.market_slab.as_ref(),
                pool.percolator_program.as_ref(),
                pool.coin_mint.as_ref(),
                &self.policy,
                &self.domain,
                &self.deposit_window_slots,
            ]),
            PoolSeedVersion::Start => seeds.extend_from_slice(&[
                pool.market_slab.as_ref(),
                pool.percolator_program.as_ref(),
                pool.coin_mint.as_ref(),
                &self.policy,
                &self.domain,
                &self.deposit_window_slots,
                &self.deposit_start_slot,
            ]),
            PoolSeedVersion::Bootstrap => seeds.extend_from_slice(&[
                pool.market_slab.as_ref(),
                pool.percolator_program.as_ref(),
                pool.coin_mint.as_ref(),
                &self.policy,
                &self.domain,
                &self.deposit_window_slots,
                &self.deposit_start_slot,
                &self.bootstrap_delay_slots,
            ]),
            PoolSeedVersion::CrossBacking => seeds.extend_from_slice(&[
                pool.market_slab.as_ref(),
                pool.percolator_program.as_ref(),
                pool.coin_mint.as_ref(),
                &self.policy,
                &self.domain,
                &self.deposit_window_slots,
                &self.deposit_start_slot,
                &self.bootstrap_delay_slots,
                CROSS_BACKING_POOL_SEED,
            ]),
        }
        seeds.push(&self.bump);
        seeds
    }
}

fn pool_seed_version(
    program_id: &Pubkey,
    pool_key: &Pubkey,
    pool_data_len: usize,
    pool: &Pool,
) -> Result<PoolSeedVersion, ProgramError> {
    const BASE: &[PoolSeedVersion] = &[PoolSeedVersion::Base];
    const BASE_OR_MARKET: &[PoolSeedVersion] = &[PoolSeedVersion::Base, PoolSeedVersion::Market];
    const MARKET: &[PoolSeedVersion] = &[PoolSeedVersion::Market];
    const COIN_OR_POLICY: &[PoolSeedVersion] =
        &[PoolSeedVersion::Coin, PoolSeedVersion::PolicyDomain];
    const WINDOW: &[PoolSeedVersion] = &[PoolSeedVersion::Window];
    const START: &[PoolSeedVersion] = &[PoolSeedVersion::Start];
    const BOOTSTRAP: &[PoolSeedVersion] = &[PoolSeedVersion::Bootstrap];
    const CROSS_BACKING: &[PoolSeedVersion] = &[PoolSeedVersion::CrossBacking];

    let candidates = if pool.cross_backing {
        if pool_data_len < POOL_SIZE_CROSS_BACKING_V1 {
            return Err(ProgramError::InvalidAccountData);
        }
        CROSS_BACKING
    } else {
        match pool_data_len {
            POOL_SIZE_BASE | POOL_SIZE_MARKET => BASE,
            POOL_SIZE_VOTE => BASE_OR_MARKET,
            POOL_SIZE_SHARES | POOL_SIZE_DEADLINE_ONLY => MARKET,
            POOL_SIZE_COIN => COIN_OR_POLICY,
            POOL_SIZE_WINDOW => WINDOW,
            POOL_SIZE_START => START,
            size if size >= POOL_SIZE => BOOTSTRAP,
            _ => return Err(ProgramError::InvalidAccountData),
        }
    };
    let seed_bytes = PoolSeedBytes::new(pool);
    for version in candidates {
        let signer_seeds = seed_bytes.signer_seeds(pool, *version);
        let base_seeds = &signer_seeds[..signer_seeds.len() - 1];
        let (expected, bump) = Pubkey::find_program_address(base_seeds, program_id);
        if expected == *pool_key && bump == pool.bump {
            return Ok(*version);
        }
    }
    Err(ProgramError::InvalidSeeds)
}

fn validate_pool_pda(
    program_id: &Pubkey,
    pool_account: &AccountInfo,
    pool: &Pool,
) -> Result<PoolSeedVersion, ProgramError> {
    pool_seed_version(program_id, pool_account.key, pool_account.data_len(), pool)
}

fn invoke_signed_for_pool<'a>(
    pool: &Pool,
    version: PoolSeedVersion,
    instruction: &Instruction,
    account_infos: &[AccountInfo<'a>],
) -> ProgramResult {
    let seed_bytes = PoolSeedBytes::new(pool);
    let signer_seeds = seed_bytes.signer_seeds(pool, version);
    invoke_signed(instruction, account_infos, &[signer_seeds.as_slice()])
}

struct Position {
    pool: Pubkey,
    owner: Pubkey,
    /// Live principal (current deposit, less any withdrawal). Genesis reads this
    /// directly: one principal base unit contributes one vote.
    principal: u64,
    /// Monotonic action nonce while the position is active. A permissionless
    /// terminal return reuses it for the principal-at-risk reward snapshot.
    withdrawn_amount: u64,
    withdrawn: bool,
    /// Last-write-time of this position (set on deposit). Topping up resets it so
    /// late additions do not inherit earlier reward tenure.
    start_slot: u64,
    /// Set by the pool's vote_authority while a genesis vote is live on this
    /// position. Blocks insurance-withdraw until the vote is retracted.
    vote_locked: bool,
    /// A permissionless terminal genesis return consumed the position. In this
    /// state `withdrawn_amount` stores the principal still at risk immediately
    /// before that return, so a cranker cannot erase its capital reward.
    terminal_returned: bool,
    /// Slot when the permissionless terminal return removed the principal. The
    /// current layout stores `slot + 1` as a five-byte little-endian integer;
    /// zero remains the legacy/no-snapshot sentinel.
    terminal_return_slot: Option<u64>,
    /// Share generation for an active position. The serialized bytes are reused
    /// for `terminal_return_slot` after terminal retirement.
    share_generation: u64,
    /// Shares held for share-value accounting. Insurance deposits mint priced
    /// shares for both payout policies so residual-distributor live caps track
    /// remaining capital; own-vault POLICY_PRINCIPAL deposits keep this at 0.
    shares: u128,
}

impl Position {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if !supported_position_size(data.len()) || data[..8] != POSITION_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let withdrawn = data[88];
        let vote_locked = if data.len() >= POSITION_SIZE_TENURE {
            data[97]
        } else {
            0
        };
        let terminal_returned = if data.len() >= POSITION_SIZE {
            data[POS_TERMINAL_RETURNED_OFF]
        } else {
            0
        };
        let generation_or_terminal_slot = if data.len() >= POSITION_SIZE {
            decode_u40(&data[POS_TERMINAL_RETURN_SLOT_OFF..POS_TERMINAL_RETURN_SLOT_OFF + 5])
        } else {
            0
        };
        let terminal_return_slot = if terminal_returned == 1 {
            generation_or_terminal_slot.checked_sub(1)
        } else {
            None
        };
        if withdrawn > 1
            || vote_locked > 1
            || terminal_returned > 1
            || (terminal_returned == 1
                && (withdrawn != 1 || u64::from_le_bytes(data[72..80].try_into().unwrap()) != 0))
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            pool: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            owner: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            principal: u64::from_le_bytes(data[72..80].try_into().unwrap()),
            withdrawn_amount: u64::from_le_bytes(data[80..88].try_into().unwrap()),
            withdrawn: withdrawn == 1,
            start_slot: if data.len() >= POSITION_SIZE_TENURE {
                u64::from_le_bytes(data[89..97].try_into().unwrap())
            } else {
                0
            },
            vote_locked: vote_locked == 1,
            terminal_returned: terminal_returned == 1,
            terminal_return_slot,
            share_generation: if terminal_returned == 0 {
                generation_or_terminal_slot
            } else {
                0
            },
            shares: if data.len() >= POSITION_SIZE {
                u128::from_le_bytes(data[104..120].try_into().unwrap())
            } else {
                0
            },
        })
    }

    fn serialize(&self, data: &mut [u8]) -> ProgramResult {
        if !supported_position_size(data.len()) {
            return Err(ProgramError::InvalidAccountData);
        }
        data[..8].copy_from_slice(&POSITION_DISC);
        data[8..40].copy_from_slice(self.pool.as_ref());
        data[40..72].copy_from_slice(self.owner.as_ref());
        data[72..80].copy_from_slice(&self.principal.to_le_bytes());
        data[80..88].copy_from_slice(&self.withdrawn_amount.to_le_bytes());
        data[88] = self.withdrawn as u8;
        if data.len() == POSITION_SIZE_BASE {
            data[89..POSITION_SIZE_BASE].fill(0);
        } else {
            data[89..97].copy_from_slice(&self.start_slot.to_le_bytes());
            data[97] = self.vote_locked as u8;
            if data.len() >= POSITION_SIZE {
                data[POS_TERMINAL_RETURNED_OFF] = self.terminal_returned as u8;
                if !self.terminal_returned && self.terminal_return_slot.is_some() {
                    return Err(ProgramError::InvalidAccountData);
                }
                let generation_or_terminal_slot = if self.terminal_returned {
                    match self.terminal_return_slot {
                        Some(slot) => slot
                            .checked_add(1)
                            .filter(|encoded| *encoded <= U40_MAX)
                            .ok_or(ProgramError::InvalidAccountData)?,
                        None => 0,
                    }
                } else {
                    self.share_generation
                };
                encode_u40(
                    generation_or_terminal_slot,
                    &mut data[POS_TERMINAL_RETURN_SLOT_OFF..POS_TERMINAL_RETURN_SLOT_OFF + 5],
                )?;
                data[104..120].copy_from_slice(&self.shares.to_le_bytes());
            } else {
                data[98..104].fill(0);
            }
        }
        Ok(())
    }
}

fn supported_position_size(size: usize) -> bool {
    matches!(size, POSITION_SIZE_BASE | POSITION_SIZE_TENURE) || size >= POSITION_SIZE
}

// ---------------------------------------------------------------------------
// Pure payout logic (the ported subledger arithmetic)
// ---------------------------------------------------------------------------

fn mul_div_floor(a: u64, b: u64, denom: u64) -> Option<u64> {
    if denom == 0 {
        return None;
    }
    Some((a as u128 * b as u128 / denom as u128) as u64)
}

/// Exact quotient and remainder for `a * b / denom` without requiring the product
/// to fit in `u128`. Returns `None` only for a zero denominator or a quotient above
/// `u128::MAX`.
fn wide_mul_div_rem(a: u128, b: u128, denom: u128) -> Option<(u128, u128)> {
    if denom == 0 {
        return None;
    }

    // a = whole * denom + rem. The whole-number contribution is exact, and
    // overflow here proves that the final non-negative quotient cannot fit.
    let whole = a / denom;
    let rem = a % denom;
    let mut quotient = whole.checked_mul(b)?;

    // Compute floor(rem * b / denom) one bit of b at a time. `remainder`
    // always stays below denom, and add_mod avoids overflowing its u128 sum.
    let mut fractional = 0u128;
    let mut remainder = 0u128;
    for bit in (0..128).rev() {
        fractional = fractional.checked_mul(2)?;
        let (next_remainder, carry) = add_mod(remainder, remainder, denom);
        remainder = next_remainder;
        fractional = fractional.checked_add(carry)?;

        if ((b >> bit) & 1) != 0 {
            let (next_remainder, carry) = add_mod(remainder, rem, denom);
            remainder = next_remainder;
            fractional = fractional.checked_add(carry)?;
        }
    }

    quotient = quotient.checked_add(fractional)?;
    Some((quotient, remainder))
}

fn wide_mul_div_floor(a: u128, b: u128, denom: u128) -> Option<u128> {
    wide_mul_div_rem(a, b, denom).map(|(quotient, _)| quotient)
}

fn wide_mul_div_ceil(a: u128, b: u128, denom: u128) -> Option<u128> {
    let (quotient, remainder) = wide_mul_div_rem(a, b, denom)?;
    quotient.checked_add(u128::from(remainder != 0))
}

fn insurance_domain_balances(balance: percolator_accounting::InsuranceAssetBalance) -> [u128; 2] {
    [
        balance.domains[0].remaining_atoms,
        balance.domains[1].remaining_atoms,
    ]
}

/// Credit `amount` toward the lower live domain balance, then split any
/// remainder evenly. Zero-value claim exits do not move these Percolator
/// balances, while market losses are repaired before new capital is split.
fn insurance_deposit_domain_delta(balances: [u128; 2], amount: u64) -> [u128; 2] {
    let amount = u128::from(amount);
    if balances[0] <= balances[1] {
        let gap = core::cmp::min(balances[1] - balances[0], amount);
        let remainder = amount - gap;
        [gap + remainder / 2, remainder - remainder / 2]
    } else {
        let gap = core::cmp::min(balances[0] - balances[1], amount);
        let remainder = amount - gap;
        [remainder / 2, gap + remainder - remainder / 2]
    }
}

/// Debit `amount` from the higher live domain balance, then split any
/// remainder evenly. On an equal balance the odd atom comes from long, exactly
/// reversing the deposit tie-break that credits it to short.
fn insurance_withdraw_domain_delta(balances: [u128; 2], amount: u64) -> [u128; 2] {
    let amount = u128::from(amount);
    if balances[0] >= balances[1] {
        let gap = core::cmp::min(balances[0] - balances[1], amount);
        let remainder = amount - gap;
        [gap + remainder - remainder / 2, remainder / 2]
    } else {
        let gap = core::cmp::min(balances[1] - balances[0], amount);
        let remainder = amount - gap;
        [remainder - remainder / 2, gap + remainder / 2]
    }
}

/// Scale one nominal insurance/backing tranche to the amount actually payable,
/// then clamp it to the live value available in each class. Healthy exits reverse
/// the fixed aggregate 50/50 allocation exactly; impaired exits cannot debit a
/// class that has already been lost.
fn protection_withdrawal_delta(
    balances: [u128; 2],
    payout: u64,
    nominal_debit: [u128; 2],
) -> Result<[u128; 2], ProgramError> {
    if payout == 0 {
        return Ok([0, 0]);
    }
    let payout = u128::from(payout);
    let available = balances[0]
        .checked_add(balances[1])
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let nominal = nominal_debit[0]
        .checked_add(nominal_debit[1])
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if payout > available || nominal == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let ideal_insurance = wide_mul_div_floor(payout, nominal_debit[0], nominal)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let minimum_insurance = payout.saturating_sub(balances[1]);
    let maximum_insurance = core::cmp::min(payout, balances[0]);
    if minimum_insurance > maximum_insurance {
        return Err(ProgramError::InvalidAccountData);
    }
    let insurance = core::cmp::max(
        minimum_insurance,
        core::cmp::min(ideal_insurance, maximum_insurance),
    );
    Ok([insurance, payout - insurance])
}

/// Percolator's asset-wide withdrawal consumes the long domain before the short
/// domain. Withdraw only far enough to reach this principal tranche's short-domain
/// debit, then put the excess long-domain amount back. Surplus follows the residual
/// domain balances. Reservation floors constrain the split only when market risk has
/// made the ideal debit unavailable.
fn insurance_withdrawal_plan(
    balance: percolator_accounting::InsuranceAssetBalance,
    payout: u128,
    principal_debit: [u128; 2],
) -> Result<InsuranceWithdrawalPlan, ProgramError> {
    let domain_total = balance.domains[0]
        .remaining_atoms
        .checked_add(balance.domains[1].remaining_atoms)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let principal_total = principal_debit[0]
        .checked_add(principal_debit[1])
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if payout == 0
        || principal_total == 0
        || payout > balance.remaining_atoms
        || payout > balance.withdrawable_atoms
        || domain_total != balance.remaining_atoms
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let long_capacity = core::cmp::min(
        balance.withdrawable_atoms,
        balance.domains[0].withdrawable_atoms,
    );
    let short_capacity = balance
        .withdrawable_atoms
        .checked_sub(long_capacity)
        .ok_or(ProgramError::InvalidAccountData)?;
    if short_capacity > balance.domains[1].withdrawable_atoms {
        return Err(ProgramError::InvalidAccountData);
    }

    let ideal_long_debit = if payout <= principal_total {
        wide_mul_div_floor(payout, principal_debit[0], principal_total)
            .ok_or(ProgramError::ArithmeticOverflow)?
    } else {
        let surplus = payout
            .checked_sub(principal_total)
            .ok_or(ProgramError::InvalidAccountData)?;
        let residual_long = balance.domains[0]
            .remaining_atoms
            .saturating_sub(principal_debit[0]);
        let residual_short = balance.domains[1]
            .remaining_atoms
            .saturating_sub(principal_debit[1]);
        let residual_total = residual_long
            .checked_add(residual_short)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        if surplus > residual_total {
            return Err(ProgramError::InvalidAccountData);
        }
        principal_debit[0]
            .checked_add(
                wide_mul_div_floor(surplus, residual_long, residual_total)
                    .ok_or(ProgramError::ArithmeticOverflow)?,
            )
            .ok_or(ProgramError::ArithmeticOverflow)?
    };
    let minimum_long_debit = payout.saturating_sub(short_capacity);
    let maximum_long_debit = core::cmp::min(payout, long_capacity);
    if minimum_long_debit > maximum_long_debit {
        return Err(ProgramError::InvalidAccountData);
    }
    let long_debit = core::cmp::max(
        minimum_long_debit,
        core::cmp::min(ideal_long_debit, maximum_long_debit),
    );
    let short_debit = payout
        .checked_sub(long_debit)
        .ok_or(ProgramError::InvalidAccountData)?;
    let target_remaining = [
        balance.domains[0]
            .remaining_atoms
            .checked_sub(long_debit)
            .ok_or(ProgramError::InvalidAccountData)?,
        balance.domains[1]
            .remaining_atoms
            .checked_sub(short_debit)
            .ok_or(ProgramError::InvalidAccountData)?,
    ];
    percolator_accounting::plan_insurance_withdrawal_to_domains(
        balance,
        payout,
        target_remaining,
    )
    .map_err(|_| ProgramError::InvalidAccountData)
}

/// `(x + y) mod denom` and `floor((x + y) / denom)` for `x,y < denom`.
fn add_mod(x: u128, y: u128, denom: u128) -> (u128, u128) {
    debug_assert!(denom > 0 && x < denom && y < denom);
    if x >= denom - y {
        (x - (denom - y), 1)
    } else {
        (x + y, 0)
    }
}

// Tenure-fair share accounting for POLICY_WITH_SURPLUS (branch residual-genesis).
// Shares are priced by the LIVE balance so a deposit only ever redeems the surplus that accrued
// during its own tenure. VIRTUAL-OFFSET inflation defense (finding HU): the pricing uses
// `total_shares + VIRTUAL_SHARES` over `balance + 1` (ERC4626-style), so the classic first-depositor
// inflation/donation rounding-skim is bounded to ~amount/VIRTUAL_SHARES. This matters because an
// own-vault pool's vault is a plain SPL token account ANYONE can donate into; without the offset a
// 1-atom first depositor could donate to inflate the share price and skim a later depositor's
// rounding. The dust the offset diverts (≤ ~1 unit/op) accrues to the never-redeemable virtual shares.
const VIRTUAL_SHARES: u128 = 1_000_000;

fn share_generation_parts(encoded: u64) -> Result<(u64, u64), ProgramError> {
    if encoded > U40_MAX {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok((
        encoded & SHARE_GENERATION_MASK,
        encoded >> SHARE_GENERATION_BITS,
    ))
}

fn encode_share_generation(reset_generation: u64, scale_epoch: u64) -> Result<u64, ProgramError> {
    if reset_generation > SHARE_GENERATION_MASK || scale_epoch > SHARE_GENERATION_MASK {
        return Err(ProgramError::ArithmeticOverflow);
    }
    Ok(reset_generation | (scale_epoch << SHARE_GENERATION_BITS))
}

fn insurance_virtual_shares(encoded_generation: u64) -> Result<u128, ProgramError> {
    let (_, scale_epoch) = share_generation_parts(encoded_generation)?;
    if scale_epoch >= VIRTUAL_SHARE_SCALE_LIMIT {
        return Ok(1);
    }
    let divisor = 1u128 << scale_epoch;
    Ok(VIRTUAL_SHARES.div_ceil(divisor))
}

fn mint_shares_with_virtual_offset(
    amount: u64,
    total_shares: u128,
    balance: u64,
    virtual_shares: u128,
) -> Result<u128, ProgramError> {
    wide_mul_div_floor(
        amount as u128,
        total_shares
            .checked_add(virtual_shares)
            .ok_or(ProgramError::ArithmeticOverflow)?,
        (balance as u128)
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?,
    )
    .ok_or(ProgramError::ArithmeticOverflow)
}

/// Shares minted for `amount`, priced by the pre-deposit `balance` with the virtual offset.
fn mint_shares(amount: u64, total_shares: u128, balance: u64) -> Result<u128, ProgramError> {
    mint_shares_with_virtual_offset(amount, total_shares, balance, VIRTUAL_SHARES)
}

/// A zero priced balance proves every outstanding share has zero token value.
/// Advance the generation before recapitalization so the finite `u128` share
/// namespace can restart without letting impaired positions claim new capital.
fn begin_fully_impaired_recapitalization(pool: &mut Pool, priced_balance: u64) -> ProgramResult {
    if priced_balance != 0 || pool.total_shares == 0 {
        return Ok(());
    }
    if pool.outstanding_principal != 0 {
        let (reset_generation, _) = share_generation_parts(pool.share_generation)?;
        let reset_generation = reset_generation
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        pool.share_generation = encode_share_generation(reset_generation, 0)?;
    }
    pool.total_shares = 0;
    Ok(())
}

fn begin_fully_impaired_indexed_recapitalization(pool: &mut Pool) -> ProgramResult {
    if pool.share_rate_numerator != 0 || pool.total_shares == 0 {
        if pool.total_shares == 0 {
            pool.share_rate_numerator = 1;
            pool.share_rate_denominator = VIRTUAL_SHARES;
        }
        return Ok(());
    }
    if pool.outstanding_principal != 0 {
        let (reset_generation, _) = share_generation_parts(pool.share_generation)?;
        pool.share_generation = encode_share_generation(
            reset_generation
                .checked_add(1)
                .ok_or(ProgramError::ArithmeticOverflow)?,
            0,
        )?;
    }
    pool.total_shares = 0;
    pool.share_rate_numerator = 1;
    pool.share_rate_denominator = VIRTUAL_SHARES;
    Ok(())
}

fn rescale_insurance_shares(pool: &mut Pool) -> ProgramResult {
    let (reset_generation, scale_epoch) = share_generation_parts(pool.share_generation)?;
    // Pool totals round up while each lazily touched position rounds down. This keeps the sum of
    // owned shares bounded by the pool and ensures scaling cannot raise a surviving holder's rate.
    let scaled_total = pool.total_shares.div_ceil(2);
    if scaled_total == 0 || scaled_total == pool.total_shares {
        return Err(ProgramError::ArithmeticOverflow);
    }
    pool.total_shares = scaled_total;
    pool.share_generation = encode_share_generation(
        reset_generation,
        scale_epoch
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?,
    )?;
    Ok(())
}

fn uses_indexed_cross_backing(pool: &Pool, pool_data_len: usize) -> bool {
    pool.cross_backing && pool_data_len >= POOL_SIZE_CROSS_BACKING_V3
}

fn uses_external_loss_checkpoints(pool: &Pool, pool_data_len: usize) -> bool {
    pool.cross_backing && pool_data_len >= POOL_SIZE_CROSS_BACKING
}

fn owner_backing_protection(pool: &Pool, protected_backing: u128) -> Result<u64, ProgramError> {
    let nominal_backing = percolator_accounting::balanced_insurance_domains(u128::from(
        pool.outstanding_principal,
    ))[1];
    u64::try_from(protected_backing.min(nominal_backing))
        .map_err(|_| ProgramError::ArithmeticOverflow)
}

fn redeem_indexed_shares(
    shares: u128,
    share_rate_numerator: u128,
    share_rate_denominator: u128,
) -> Result<u64, ProgramError> {
    let value = wide_mul_div_floor(shares, share_rate_numerator, share_rate_denominator)
        .ok_or(ProgramError::InvalidAccountData)?;
    u64::try_from(value).map_err(|_| ProgramError::ArithmeticOverflow)
}

fn wide_product(left: u128, right: u128) -> Result<(u128, u128), ProgramError> {
    const LIMB_MASK: u128 = u64::MAX as u128;
    let left_low = left & LIMB_MASK;
    let left_high = left >> 64;
    let right_low = right & LIMB_MASK;
    let right_high = right >> 64;

    let low_product = left_low * right_low;
    let low_word = low_product & LIMB_MASK;
    let middle = left_high
        .checked_mul(right_low)
        .and_then(|value| value.checked_add(low_product >> 64))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let middle_low = middle & LIMB_MASK;
    let middle_high = middle >> 64;
    let upper_middle = left_low
        .checked_mul(right_high)
        .and_then(|value| value.checked_add(middle_low))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let low = (upper_middle << 64) | low_word;
    let high = left_high
        .checked_mul(right_high)
        .and_then(|value| value.checked_add(middle_high))
        .and_then(|value| value.checked_add(upper_middle >> 64))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    Ok((high, low))
}

fn compare_fractions(
    left_numerator: u128,
    left_denominator: u128,
    right_numerator: u128,
    right_denominator: u128,
) -> Result<core::cmp::Ordering, ProgramError> {
    if left_denominator == 0 || right_denominator == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(wide_product(left_numerator, right_denominator)?
        .cmp(&wide_product(right_numerator, left_denominator)?))
}

/// Current cross-backed pools use a loss-only share-rate accumulator. Physical
/// protection above the aggregate indexed claim is protocol value and cannot
/// raise the rate; a real market loss lowers it before the next user action.
/// Exact share burns and floor-priced mints make the live-balance candidate at
/// least the stored rate after any user exit or deposit, respectively.
fn sync_indexed_share_rate(pool: &mut Pool, protected_balance: u64) -> ProgramResult {
    if pool.total_shares == 0 || pool.share_rate_numerator == 0 {
        return Ok(());
    }
    if pool.share_rate_denominator == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    if protected_balance == 0 {
        pool.share_rate_numerator = 0;
        pool.share_rate_denominator = 1;
        return Ok(());
    }
    let candidate_numerator = (protected_balance as u128)
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let candidate_denominator = pool
        .total_shares
        .checked_add(insurance_virtual_shares(pool.share_generation)?)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if compare_fractions(
        candidate_numerator,
        candidate_denominator,
        pool.share_rate_numerator,
        pool.share_rate_denominator,
    )? == core::cmp::Ordering::Less
    {
        pool.share_rate_numerator = candidate_numerator;
        pool.share_rate_denominator = candidate_denominator;
    }
    Ok(())
}

fn indexed_protected_balance_after_external_loss(
    pool: &Pool,
    protected_balance: u64,
    observed_insurance_spent: u128,
    observed_backing_protected: u64,
) -> Result<u64, ProgramError> {
    let new_insurance_loss = observed_insurance_spent
        .checked_sub(pool.insurance_spent_checkpoint)
        .ok_or(ProgramError::InvalidAccountData)?;
    let new_backing_loss = pool
        .backing_protected_checkpoint
        .saturating_sub(observed_backing_protected);
    let new_loss = new_insurance_loss
        .checked_add(u128::from(new_backing_loss))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if pool.total_shares == 0 {
        return Ok(0);
    }
    let indexed_claim = redeem_indexed_shares(
        pool.total_shares,
        pool.share_rate_numerator,
        pool.share_rate_denominator,
    )?;
    let loss_cap = u128::from(indexed_claim)
        .saturating_sub(new_loss)
        .min(u128::from(pool.outstanding_principal));
    u64::try_from(core::cmp::min(
        u128::from(protected_balance),
        loss_cap,
    ))
    .map_err(|_| ProgramError::ArithmeticOverflow)
}

fn sync_indexed_cross_backing_external_loss(
    pool: &mut Pool,
    protected_balance: u64,
    observed_insurance_spent: u128,
    observed_backing_protected: u64,
) -> ProgramResult {
    let new_insurance_loss = observed_insurance_spent
        .checked_sub(pool.insurance_spent_checkpoint)
        .ok_or(ProgramError::InvalidAccountData)?;
    let new_backing_loss = pool
        .backing_protected_checkpoint
        .saturating_sub(observed_backing_protected);
    let new_loss = new_insurance_loss
        .checked_add(u128::from(new_backing_loss))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let protected = if new_loss == 0 {
        // Deposits and exact share exits can leave the aggregate real-share
        // redemption one floor atom below the stored rational rate. Feeding that
        // floor back as a new cap would ratchet surviving claims on every no-loss
        // action. Physical synchronization is already monotonic and exit-safe.
        protected_balance
    } else {
        indexed_protected_balance_after_external_loss(
            pool,
            protected_balance,
            observed_insurance_spent,
            observed_backing_protected,
        )?
    };
    sync_indexed_share_rate(pool, protected)?;
    pool.insurance_spent_checkpoint = observed_insurance_spent;
    pool.backing_protected_checkpoint = observed_backing_protected;
    Ok(())
}

fn aggregate_insurance_spent(spent: [u128; 2]) -> Result<u128, ProgramError> {
    spent
        .into_iter()
        .try_fold(0u128, |total, value| total.checked_add(value))
        .ok_or(ProgramError::ArithmeticOverflow)
}

fn mint_indexed_shares_with_capacity(
    pool: &mut Pool,
    amount: u64,
) -> Result<u128, ProgramError> {
    if pool.share_rate_numerator == 0 || pool.share_rate_denominator == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let shares = wide_mul_div_floor(
        amount as u128,
        pool.share_rate_denominator,
        pool.share_rate_numerator,
    )
    .ok_or(ProgramError::ArithmeticOverflow)?;
    let virtual_shares = insurance_virtual_shares(pool.share_generation)?;
    // Never rescale live owner shares to admit a deposit: any lazy integer
    // rescale can erase an independent holder's whole-atom claim. Capacity
    // failure is detected before the user's token transfer and leaves all
    // existing custody and withdrawal paths unchanged.
    if shares == 0
        || pool
            .total_shares
            .checked_add(shares)
            .and_then(|total| total.checked_add(virtual_shares))
            .is_none()
    {
        return Err(ProgramError::ArithmeticOverflow);
    }
    Ok(shares)
}

fn require_bounded_indexed_rounding(
    amount: u64,
    shares_minted: u128,
    share_rate_numerator: u128,
    share_rate_denominator: u128,
) -> ProgramResult {
    let immediate_value = redeem_indexed_shares(
        shares_minted,
        share_rate_numerator,
        share_rate_denominator,
    )?;
    if immediate_value == 0
        || immediate_value > amount
        || amount.saturating_sub(immediate_value) > 1
    {
        return Err(ProgramError::InvalidArgument);
    }
    Ok(())
}

fn mint_insurance_shares_with_capacity(
    pool: &mut Pool,
    amount: u64,
    balance: u64,
) -> Result<(u128, u128), ProgramError> {
    let denominator = (balance as u128)
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let growth_denominator = denominator
        .checked_add(amount as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    // If basis <= MAX*denominator/(denominator+amount), then
    // basis + floor(amount*basis/denominator) fits u128. Compute that exact threshold once;
    // subsequent retries are only fixed-width shifts, not attacker-amplified wide divisions.
    let maximum_basis = wide_mul_div_floor(u128::MAX, denominator, growth_denominator)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    let mut rescaled = false;
    for _ in 0..=u128::BITS {
        let virtual_shares = insurance_virtual_shares(pool.share_generation)?;
        let Some(basis) = pool.total_shares.checked_add(virtual_shares) else {
            rescale_insurance_shares(pool)?;
            rescaled = true;
            continue;
        };
        if basis <= maximum_basis {
            // Rounding the pool total up during a capacity rescale protects existing holders.
            // Round this first post-rescale mint up as well so a minimum deposit retains one atom;
            // at this scale the possible extra share is itself worth less than one atom.
            let shares = if rescaled {
                wide_mul_div_ceil(amount as u128, basis, denominator)
                    .ok_or(ProgramError::ArithmeticOverflow)?
            } else {
                mint_shares_with_virtual_offset(
                    amount,
                    pool.total_shares,
                    balance,
                    virtual_shares,
                )?
            };
            if pool
                .total_shares
                .checked_add(shares)
                .and_then(|total| total.checked_add(virtual_shares))
                .is_some()
            {
                return Ok((shares, virtual_shares));
            }
        }
        rescale_insurance_shares(pool)?;
        rescaled = true;
    }
    Err(ProgramError::ArithmeticOverflow)
}

fn move_position_to_current_share_generation(
    position: &mut Position,
    pool: &Pool,
) -> Result<bool, ProgramError> {
    let (position_reset, position_scale) = share_generation_parts(position.share_generation)?;
    let (pool_reset, pool_scale) = share_generation_parts(pool.share_generation)?;
    if position_reset > pool_reset || (position_reset == pool_reset && position_scale > pool_scale) {
        return Err(ProgramError::InvalidAccountData);
    }
    let reset_generation_is_current = position_reset == pool_reset;
    if position_reset < pool_reset {
        position.shares = 0;
    } else if position_scale < pool_scale {
        let shift = pool_scale - position_scale;
        position.shares = if shift >= u128::BITS as u64 {
            0
        } else {
            position.shares >> shift
        };
    }
    position.share_generation = pool.share_generation;
    Ok(reset_generation_is_current)
}

fn redeem_shares_with_virtual_offset(
    shares: u128,
    balance: u64,
    total_shares: u128,
    virtual_shares: u128,
) -> Result<u64, ProgramError> {
    let owed = wide_mul_div_floor(
        shares,
        (balance as u128)
            .checked_add(1)
            .ok_or(ProgramError::ArithmeticOverflow)?,
        total_shares
            .checked_add(virtual_shares)
            .ok_or(ProgramError::ArithmeticOverflow)?,
    )
    .ok_or(ProgramError::ArithmeticOverflow)?;
    u64::try_from(owed).map_err(|_| ProgramError::ArithmeticOverflow)
}

/// Tokens redeemed for `shares`: `shares * (balance + 1) / (total_shares + VIRTUAL_SHARES)` (floor).
fn redeem_shares(shares: u128, balance: u64, total_shares: u128) -> Result<u64, ProgramError> {
    redeem_shares_with_virtual_offset(shares, balance, total_shares, VIRTUAL_SHARES)
}

fn rate_safe_pool_share_burn_with_virtual_offset(
    shares_retired: u128,
    balance_before: u64,
    balance_after: u64,
    total_shares: u128,
    virtual_shares: u128,
) -> Result<u128, ProgramError> {
    if shares_retired > total_shares || balance_after > balance_before {
        return Err(ProgramError::InvalidAccountData);
    }
    let denominator_before = total_shares
        .checked_add(virtual_shares)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let numerator_before = (balance_before as u128)
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let numerator_after = (balance_after as u128)
        .checked_add(1)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let minimum_denominator_after = wide_mul_div_ceil(
        numerator_after,
        denominator_before,
        numerator_before,
    )
    .ok_or(ProgramError::ArithmeticOverflow)?;
    let maximum_burn = denominator_before
        .checked_sub(minimum_denominator_after)
        .ok_or(ProgramError::InvalidAccountData)?;
    Ok(core::cmp::min(shares_retired, maximum_burn))
}

/// Burn no more pricing shares than keeps the post-redemption exchange rate from
/// increasing. Retired shares above this amount become unowned rounding reserve,
/// preventing a later exiter from collecting earlier holders' floored claims.
fn rate_safe_pool_share_burn(
    shares_retired: u128,
    balance_before: u64,
    balance_after: u64,
    total_shares: u128,
) -> Result<u128, ProgramError> {
    rate_safe_pool_share_burn_with_virtual_offset(
        shares_retired,
        balance_before,
        balance_after,
        total_shares,
        VIRTUAL_SHARES,
    )
}

fn pool_total_shares_after_exit(
    policy: u8,
    outstanding_after: u64,
    priced_balance_after: u64,
    total_shares: u128,
    shares_burned: u128,
) -> Result<u128, ProgramError> {
    if outstanding_after == 0 {
        return match policy {
            POLICY_PRINCIPAL => Ok(0),
            POLICY_WITH_SURPLUS => (priced_balance_after as u128)
                .checked_mul(VIRTUAL_SHARES)
                .ok_or(ProgramError::ArithmeticOverflow),
            _ => Err(ProgramError::InvalidAccountData),
        };
    }
    total_shares
        .checked_sub(shares_burned)
        .ok_or(ProgramError::InvalidAccountData)
}

/// Reject a deposit before transfer when its shares lose more than one atom to entry rounding at
/// the post-deposit price. The virtual offset intentionally absorbs bounded rounding dust, but it
/// must not make an accepted position materially under-valued before it takes any market risk.
fn require_bounded_share_rounding(
    amount: u64,
    shares_minted: u128,
    total_shares: u128,
    balance_before: u64,
) -> ProgramResult {
    require_bounded_share_rounding_with_virtual_offset(
        amount,
        shares_minted,
        total_shares,
        balance_before,
        VIRTUAL_SHARES,
    )
}

fn require_bounded_share_rounding_with_virtual_offset(
    amount: u64,
    shares_minted: u128,
    total_shares: u128,
    balance_before: u64,
    virtual_shares: u128,
) -> ProgramResult {
    if shares_minted == 0 {
        return Err(ProgramError::InvalidArgument);
    }
    let post_balance = balance_before
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let post_total_shares = total_shares
        .checked_add(shares_minted)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let immediate_value = redeem_shares_with_virtual_offset(
        shares_minted,
        post_balance,
        post_total_shares,
        virtual_shares,
    )?;
    if immediate_value == 0 || amount.saturating_sub(immediate_value) > 1 {
        return Err(ProgramError::InvalidArgument);
    }
    Ok(())
}

/// Payout for a full position exit. `balance` is the pool's live token balance.
fn payout(policy: u8, balance: u64, outstanding: u64, principal: u64) -> Result<u64, ProgramError> {
    if outstanding == 0 || principal == 0 || principal > outstanding {
        return Err(ProgramError::InvalidAccountData);
    }
    let pro_rata =
        mul_div_floor(balance, principal, outstanding).ok_or(ProgramError::ArithmeticOverflow)?;
    match policy {
        POLICY_PRINCIPAL => {
            if balance >= outstanding {
                Ok(principal) // healthy: principal only, surplus stays in the pool
            } else {
                Ok(pro_rata) // impaired: pro-rata
            }
        }
        POLICY_WITH_SURPLUS => Ok(pro_rata), // always pro-rata: yield returned
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// Shared offsets are derived from the exact Cargo-pinned engine structs. The LiteSVM canary
// also pins the wrapper-owned prefix against the real pinned Percolator program.
pub const PERC_MARKET_GROUP_OFFSET: usize = percolator_accounting::MARKET_GROUP_OFFSET;
pub const PERC_INSURANCE_OFFSET: usize = percolator_accounting::INSURANCE_OFFSET;

/// Asset 0's live domain-budget remainder. The market header is global across all assets,
/// so using it directly would let foreign insurance hide an asset-0 impairment.
fn read_asset0_insurance(slab_data: &[u8]) -> Result<u64, ProgramError> {
    let v = percolator_accounting::read_asset_insurance_remaining(slab_data, 0)
        .map_err(|_| ProgramError::InvalidAccountData)?;
    Ok(u64::try_from(v).unwrap_or(u64::MAX))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let (tag, mut data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        IX_INIT_POOL => process_init_pool(program_id, accounts, &mut data),
        IX_DEPOSIT => process_deposit(program_id, accounts, &mut data),
        IX_WITHDRAW => process_withdraw(program_id, accounts, &mut data),
        IX_INIT_INSURANCE_POOL => {
            process_init_insurance_pool(program_id, accounts, &mut data, false)
        }
        IX_INSURANCE_DEPOSIT => process_insurance_deposit(program_id, accounts, &mut data),
        IX_INSURANCE_WITHDRAW => process_insurance_withdraw(program_id, accounts, &mut data),
        IX_SET_VOTE_LOCK => process_set_vote_lock(program_id, accounts, &mut data),
        IX_ACCEPT_OPERATOR => process_accept_operator(program_id, accounts, &mut data),
        IX_HANDOFF_TO_TWAP => process_handoff_to_twap(program_id, accounts, &mut data),
        IX_RETURN_RESOLVED_ASSET0_BACKING => {
            process_return_resolved_asset0_backing(program_id, accounts, &mut data)
        }
        IX_ASSERT_NO_PRINCIPAL => process_assert_no_principal(program_id, accounts, &mut data),
        IX_ASSERT_PRINCIPAL => process_assert_principal(program_id, accounts, &mut data),
        IX_RETURN_FINALIZED_POSITION => {
            process_return_finalized_position(program_id, accounts, &mut data)
        }
        IX_INSURANCE_WITHDRAW_FULL => {
            process_insurance_withdraw_full(program_id, accounts, &mut data)
        }
        IX_INIT_CROSS_BACKING_GENESIS_POOL => {
            process_init_insurance_pool(program_id, accounts, &mut data, true)
        }
        IX_ROUTE_CROSS_BACKING_EARNINGS => {
            process_route_cross_backing_earnings(program_id, accounts, &mut data)
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn read_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (head, tail) = data.split_at(8);
    *data = tail;
    Ok(u64::from_le_bytes(head.try_into().unwrap()))
}

fn read_u8(data: &mut &[u8]) -> Result<u8, ProgramError> {
    if data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (head, tail) = data.split_at(1);
    *data = tail;
    Ok(head[0])
}

fn token_balance(account: &AccountInfo) -> Result<u64, ProgramError> {
    if account.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(spl_token::state::Account::unpack(&account.try_borrow_data()?)?.amount)
}

fn validate_owner_token_destination(
    account: &AccountInfo,
    expected_owner: &Pubkey,
    expected_mint: &Pubkey,
) -> ProgramResult {
    if account.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let token = spl_token::state::Account::unpack(&account.try_borrow_data()?)?;
    if token.state != spl_token::state::AccountState::Initialized
        || token.owner != *expected_owner
        || token.mint != *expected_mint
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn validate_insurance_holding(
    pool_account: &AccountInfo,
    pool: &Pool,
    holding: &AccountInfo,
) -> Result<u64, ProgramError> {
    if holding.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let token = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?;
    if token.state != spl_token::state::AccountState::Initialized
        || token.owner != *pool_account.key
        || token.mint != pool.mint
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if pool.cross_backing
        && (*holding.key != canonical_vault_address(pool_account.key, &pool.mint)
            || token.delegate.is_some()
            || token.delegated_amount != 0
            || token.close_authority.is_some()
            || token.is_native.is_some())
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(token.amount)
}

// init_pool accounts: [payer(s,w), mint, pool(w,pda), vault(token acct, authority=pool pda),
//                      system_program]
// data: asset_id (u64), policy (u8)
fn process_init_pool(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let asset_id = read_u64(data)?;
    let policy = read_u8(data)?;
    let domain = read_u8(data)?;
    if policy > POLICY_WITH_SURPLUS || domain > DOMAIN_BACKING || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Own-vault pools have no percolator market, so the market-binding seed components
    // are the default key (matching what the Pool stores below).
    let no_market = Pubkey::default();
    let asset_id_bytes = asset_id.to_le_bytes();
    let policy_seed = [policy];
    let domain_seed = [domain];
    let deposit_window_seed = OWN_VAULT_DEPOSIT_WINDOW_SLOTS.to_le_bytes();
    let deposit_start_seed = OWN_VAULT_DEPOSIT_START_SLOT.to_le_bytes();
    let bootstrap_delay_seed = OWN_VAULT_BOOTSTRAP_DELAY_SLOTS.to_le_bytes();
    let (expected_pool, bump) = Pubkey::find_program_address(
        &pool_seeds_full(
            mint.key,
            &asset_id_bytes,
            &no_market,
            &no_market,
            &no_market,
            &policy_seed,
            &domain_seed,
            &deposit_window_seed,
            &deposit_start_seed,
            &bootstrap_delay_seed,
        ),
        program_id,
    );
    if *pool_account.key != expected_pool {
        return Err(ProgramError::InvalidSeeds);
    }
    if pool_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }

    // The vault must be an SPL token account for `mint`, whose authority is the
    // pool PDA — so only this program (signing as the pool) can move funds out.
    // Require SPL Token ownership BEFORE unpacking: Account::unpack verifies bytes, NOT the owning program, so a
    // NON-SPL account with token-shaped bytes would otherwise pass the field checks. init_pool PERSISTS pool.vault
    // (permissionless PDA), so without this a front-runner could squat the pool PDA with a fake (non-SPL) vault,
    // permanently bricking the pool (every deposit then fails on the bound fake). Parity with distribution:342
    // and the rd freeze fix.
    if vault.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let vault_state = spl_token::state::Account::unpack(&vault.try_borrow_data()?)?;
    if vault_state.state != spl_token::state::AccountState::Initialized
        || vault_state.mint != *mint.key
        || vault_state.owner != expected_pool
        || vault_state.delegate.is_some()
        || vault_state.delegated_amount != 0
        || vault_state.close_authority.is_some()
        || vault_state.is_native.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let bump_arr = [bump];
    let seeds: [&[u8]; 12] = [
        b"subledger_pool",
        mint.key.as_ref(),
        &asset_id_bytes,
        no_market.as_ref(),
        no_market.as_ref(),
        no_market.as_ref(),
        &policy_seed,
        &domain_seed,
        &deposit_window_seed,
        &deposit_start_seed,
        &bootstrap_delay_seed,
        &bump_arr,
    ];
    create_pda_robust(
        payer,
        pool_account,
        system_program,
        program_id,
        &seeds,
        POOL_SIZE,
    )?;

    let pool = Pool {
        mint: *mint.key,
        asset_id,
        vault: *vault.key,
        outstanding_principal: 0,
        policy,
        domain,
        bump,
        share_generation: 0,
        market_slab: Pubkey::default(),
        percolator_program: Pubkey::default(),
        vote_authority: Pubkey::default(),
        total_shares: 0,
        coin_mint: Pubkey::default(),
        deposit_deadline_slot: u64::MAX,
        deposit_window_slots: OWN_VAULT_DEPOSIT_WINDOW_SLOTS,
        deposit_start_slot: OWN_VAULT_DEPOSIT_START_SLOT,
        bootstrap_delay_slots: OWN_VAULT_BOOTSTRAP_DELAY_SLOTS,
        cross_backing: false,
        custody_granted: false,
        custody_grant_slot_plus_one: 0,
        pending_backing: [0, 0],
        share_rate_numerator: 0,
        share_rate_denominator: 0,
        insurance_spent_checkpoint: 0,
        backing_protected_checkpoint: 0,
    };
    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    Ok(())
}

// deposit accounts: [owner(s,w), pool(w), position(w,pda), owner_ata(w), vault(w),
//                    token_program, system_program]
// data: amount (u64)
fn process_deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let owner_ata = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    // Type guard: the own-vault path must NOT touch an insurance pool. An
    // insurance pool's `vault` is the percolator insurance vault (owned by the
    // percolator vault_authority, not this pool PDA). An own-vault deposit here
    // would push funds into that vault WITHOUT a TopUpInsurance CPI — percolator
    // never counts them — and the own-vault withdraw could never sign them back
    // out, stranding the depositor's funds. Insurance pools use tags 4/5 only.
    if pool.is_insurance() {
        return Err(ProgramError::InvalidAccountData);
    }
    // Pre-share pools used pro-rata accounting and cannot persist the current
    // share fields. Keep their existing owners withdrawable, but do not accept a
    // deposit that would be serialized without its ownership attribution.
    if pool_account.data_len() < POOL_SIZE_SHARES {
        return Err(ProgramError::InvalidAccountData);
    }
    if *vault.key != pool.vault {
        return Err(ProgramError::InvalidAccountData);
    }

    // Position PDA (one per owner per pool).
    let pos_seeds = position_seeds(pool_account.key, owner.key);
    let (expected_pos, pos_bump) = Pubkey::find_program_address(&pos_seeds, program_id);
    if *position_account.key != expected_pos {
        return Err(ProgramError::InvalidSeeds);
    }
    let mut position = if position_account.data_len() == 0 {
        let bump_arr = [pos_bump];
        let seeds: [&[u8]; 4] = [
            b"subledger_position",
            pool_account.key.as_ref(),
            owner.key.as_ref(),
            &bump_arr,
        ];
        create_pda_robust(
            owner,
            position_account,
            system_program,
            program_id,
            &seeds,
            POSITION_SIZE,
        )?;
        Position {
            pool: *pool_account.key,
            owner: *owner.key,
            principal: 0,
            withdrawn_amount: 0,
            withdrawn: false,
            start_slot: 0,
            vote_locked: false,
            terminal_returned: false,
            terminal_return_slot: None,
            share_generation: pool.share_generation,
            shares: 0,
        }
    } else {
        if position_account.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let p = Position::deserialize(&position_account.try_borrow_data()?)?;
        if position_account.data_len() < POSITION_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if p.owner != *owner.key || p.pool != *pool_account.key {
            return Err(ProgramError::InvalidAccountData);
        }
        if p.withdrawn {
            return Err(ProgramError::InvalidAccountData);
        }
        p
    };

    // Tenure-fair shares (POLICY_WITH_SURPLUS, finding HT): price this deposit by the LIVE vault
    // balance BEFORE the pull, so a late depositor can only ever redeem surplus accrued during its own
    // tenure (matches the insurance path + the documented share model; POLICY_PRINCIPAL mints none).
    let shares_minted = if pool.policy == POLICY_WITH_SURPLUS {
        let balance_before = token_balance(vault)?;
        let s = mint_shares(amount, pool.total_shares, balance_before)?;
        require_bounded_share_rounding(amount, s, pool.total_shares, balance_before)?;
        s
    } else {
        0
    };

    // Pull principal into the vault (owner-signed).
    invoke(
        &spl_token::instruction::transfer(
            token_program.key,
            owner_ata.key,
            vault.key,
            owner.key,
            &[],
            amount,
        )?,
        &[
            owner_ata.clone(),
            vault.clone(),
            owner.clone(),
            token_program.clone(),
        ],
    )?;

    pool.outstanding_principal = pool
        .outstanding_principal
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    position.principal = position
        .principal
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pool.total_shares = pool
        .total_shares
        .checked_add(shares_minted)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    position.shares = position
        .shares
        .checked_add(shares_minted)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    // Last-write-time: topping up resets the vote clock, so late additions don't
    // earn early-join weight.
    position.start_slot = Clock::get()?.slot;
    advance_position_nonce(&mut position);

    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

// withdraw accounts: [owner(s,w), pool(w), position(w), owner_ata(w), vault(w), token_program]
// data: expected_principal(u64) | expected_start_slot(u64) | expected_action_nonce(u64)
fn process_withdraw(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let owner_ata = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if pool_account.owner != program_id || position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    let mut position = Position::deserialize(&position_account.try_borrow_data()?)?;
    // Type guard: own-vault withdraw must never run against an insurance pool
    // (its vault is the percolator insurance vault; the pool PDA is not its token
    // authority, so this would fail anyway — reject early and explicitly). See
    // the matching guard in the own-vault deposit. Insurance uses tags 4/5.
    if pool.is_insurance() {
        return Err(ProgramError::InvalidAccountData);
    }
    require_position_snapshot(data, &position)?;

    // Match the exact historical PDA schema implied by this deployed account.
    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    if *vault.key != pool.vault {
        return Err(ProgramError::InvalidAccountData);
    }
    // Owner-bound: only the position owner can exit, exactly once.
    if position.owner != *owner.key || position.pool != *pool_account.key {
        return Err(ProgramError::IllegalOwner);
    }
    if position.withdrawn || position.principal == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    if pool.outstanding_principal == 0 || position.principal > pool.outstanding_principal {
        return Err(ProgramError::InvalidAccountData);
    }
    validate_owner_token_destination(owner_ata, owner.key, &pool.mint)?;

    let balance = token_balance(vault)?;
    // POLICY_WITH_SURPLUS redeems the position's SHARES at the live balance (tenure-fair, finding HT):
    // shares were priced at deposit, so a late depositor only redeems its own-tenure surplus. A
    // full own-vault exit burns all of the position's shares. POLICY_PRINCIPAL keeps the pro-rata/
    // principal payout.
    let share_accounting = pool_account.data_len() >= POOL_SIZE_SHARES;
    if share_accounting && position_account.data_len() < POSITION_SIZE {
        return Err(ProgramError::InvalidAccountData);
    }
    let (paid, shares_to_retire) = if pool.policy == POLICY_WITH_SURPLUS && share_accounting {
        let shares = position.shares;
        (redeem_shares(shares, balance, pool.total_shares)?, shares)
    } else {
        (
            payout(
                pool.policy,
                balance,
                pool.outstanding_principal,
                position.principal,
            )?,
            0u128,
        )
    };
    let outstanding_after = pool
        .outstanding_principal
        .checked_sub(position.principal)
        .ok_or(ProgramError::InvalidAccountData)?;
    let balance_after = balance
        .checked_sub(paid)
        .ok_or(ProgramError::InvalidAccountData)?;
    let pool_shares_to_burn = if shares_to_retire == 0 {
        0
    } else {
        rate_safe_pool_share_burn(
            shares_to_retire,
            balance,
            balance_after,
            pool.total_shares,
        )?
    };

    if paid > 0 {
        invoke_signed_for_pool(
            &pool,
            pool_seed_version,
            &spl_token::instruction::transfer(
                token_program.key,
                vault.key,
                owner_ata.key,
                pool_account.key,
                &[],
                paid,
            )?,
            &[
                vault.clone(),
                owner_ata.clone(),
                pool_account.clone(),
                token_program.clone(),
            ],
        )?;
    }

    // A zero-payout exit still retires the position so an impaired/empty pool
    // cannot be replayed to distort other depositors' outstanding accounting.
    pool.outstanding_principal = outstanding_after;
    pool.total_shares = pool_total_shares_after_exit(
        pool.policy,
        outstanding_after,
        balance_after,
        pool.total_shares,
        pool_shares_to_burn,
    )?;
    position.shares = 0;
    position.withdrawn = true;
    advance_position_nonce(&mut position);

    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Percolator-insurance pools
// ---------------------------------------------------------------------------
//
// A pool whose `vault` is the Percolator market's canonical insurance vault. The
// pool PDA is the asset-0 insurance *authority* (so it may TopUpInsurance) and the
// asset-0 insurance *operator* (so it may WithdrawInsuranceLimited). Principal is
// custodied by Percolator, never by this program; the only way out is the
// owner-authorized, principal-only exit, capped at the owner's own recorded
// principal — the pool can never take a depositor's funds.

fn perc_vault_authority(market_slab: &Pubkey, percolator_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market_slab.as_ref()], percolator_program).0
}

/// Create a program-owned PDA, tolerating an attacker pre-funding the (deterministic) address.
/// System `create_account` aborts with AccountAlreadyInUse on ANY pre-existing lamports, so a 1-
/// lamport transfer to the address — which needs no signature — would PERMANENTLY brick init (the
/// lamports can never be swept from a system-owned PDA). Instead top up the rent shortfall (a plain
/// transfer) then allocate + assign via invoke_signed; allocate/assign only require the account to be
/// data-empty + system-owned, both true for a merely pre-funded address. Callers must still reject an
/// already-initialized account up front via `data_len() != 0` (NOT `lamports() != 0`). (finding AI)
fn create_pda_robust<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    program_id: &Pubkey,
    seeds: &[&[u8]],
    size: usize,
) -> ProgramResult {
    create_pda_robust_for_owner(
        payer,
        account,
        system_program,
        program_id,
        seeds,
        size,
    )
}

fn create_pda_robust_for_owner<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    account_owner: &Pubkey,
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
        &system_instruction::assign(account.key, account_owner),
        &[account.clone(), system_program.clone()],
        &[seeds],
    )?;
    Ok(())
}

fn backing_ledger_pda(program_id: &Pubkey, pool: &Pubkey, domain: u16) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[BACKING_LEDGER_SEED, pool.as_ref(), &domain.to_le_bytes()],
        program_id,
    )
}

fn validate_backing_ledger(
    program_id: &Pubkey,
    pool: &Pubkey,
    market_slab: &Pubkey,
    percolator_program: &Pubkey,
    ledger: &AccountInfo,
    domain: u16,
) -> Result<u128, ProgramError> {
    let expected = backing_ledger_pda(program_id, pool, domain).0;
    if *ledger.key != expected
        || ledger.owner != percolator_program
        || ledger.data_len() < percolator_accounting::backing_domain_ledger_account_len()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let data = ledger.try_borrow_data()?;
    let Some(provider) = percolator_accounting::read_backing_domain_ledger(&data)
        .map_err(|_| ProgramError::InvalidAccountData)?
    else {
        return Ok(0);
    };
    if provider.market_group != market_slab.to_bytes()
        || provider.authority != pool.to_bytes()
        || provider.domain != domain
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(provider.total_principal_atoms)
}

#[allow(clippy::too_many_arguments)]
fn bind_backing_ledger_if_needed<'a>(
    program_id: &Pubkey,
    pool: &Pool,
    pool_seed_version: PoolSeedVersion,
    pool_account: &AccountInfo<'a>,
    market_slab: &AccountInfo<'a>,
    percolator_program: &AccountInfo<'a>,
    ledger: &AccountInfo<'a>,
    domain: u16,
) -> Result<u128, ProgramError> {
    let expected = backing_ledger_pda(program_id, pool_account.key, domain).0;
    if *ledger.key != expected
        || ledger.data_len() < percolator_accounting::backing_domain_ledger_account_len()
    {
        return Err(ProgramError::InvalidAccountData);
    }

    // New pools quarantine zeroed ledgers under Subledger ownership. Percolator
    // therefore cannot bind a deterministic ledger to a foreign market before
    // the pool has acquired the configured backing authority. Existing blank
    // Percolator-owned ledgers remain upgrade-compatible with the same sync path.
    if ledger.owner == program_id {
        if ledger.try_borrow_data()?.iter().any(|byte| *byte != 0) {
            return Err(ProgramError::InvalidAccountData);
        }
        ledger.assign(percolator_program.key);
    } else if ledger.owner != percolator_program.key {
        return Err(ProgramError::IllegalOwner);
    }

    let initialized = percolator_accounting::read_backing_domain_ledger(&ledger.try_borrow_data()?)
        .map_err(|_| ProgramError::InvalidAccountData)?
        .is_some();
    if !initialized {
        let mut ix_data = vec![PERC_IX_SYNC_BACKING_DOMAIN_LEDGER];
        ix_data.extend_from_slice(&domain.to_le_bytes());
        invoke_signed_for_pool(
            pool,
            pool_seed_version,
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*pool_account.key, true),
                    AccountMeta::new(*market_slab.key, false),
                    AccountMeta::new(*ledger.key, false),
                ],
                data: ix_data,
            },
            &[
                pool_account.clone(),
                market_slab.clone(),
                ledger.clone(),
                percolator_program.clone(),
            ],
        )?;
    }

    validate_backing_ledger(
        program_id,
        pool_account.key,
        market_slab.key,
        percolator_program.key,
        ledger,
        domain,
    )
}

#[allow(clippy::too_many_arguments)]
fn withdraw_cross_backing_earnings<'a>(
    pool: &Pool,
    pool_seed_version: PoolSeedVersion,
    pool_account: &AccountInfo<'a>,
    market_slab: &AccountInfo<'a>,
    holding: &AccountInfo<'a>,
    percolator_vault: &AccountInfo<'a>,
    vault_authority: &AccountInfo<'a>,
    backing_ledgers: [&AccountInfo<'a>; 2],
    percolator_program: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    earnings: [u128; 2],
) -> ProgramResult {
    for (domain, amount) in earnings.into_iter().enumerate() {
        if amount == 0 {
            continue;
        }
        let mut ix_data = vec![PERC_IX_WITHDRAW_BACKING_BUCKET_EARNINGS];
        ix_data.extend_from_slice(&(domain as u16).to_le_bytes());
        ix_data.extend_from_slice(&amount.to_le_bytes());
        invoke_signed_for_pool(
            pool,
            pool_seed_version,
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*pool_account.key, true),
                    AccountMeta::new(*market_slab.key, false),
                    AccountMeta::new(*backing_ledgers[domain].key, false),
                    AccountMeta::new(*holding.key, false),
                    AccountMeta::new(*percolator_vault.key, false),
                    AccountMeta::new_readonly(*vault_authority.key, false),
                    AccountMeta::new_readonly(*token_program.key, false),
                ],
                data: ix_data,
            },
            &[
                pool_account.clone(),
                market_slab.clone(),
                backing_ledgers[domain].clone(),
                holding.clone(),
                percolator_vault.clone(),
                vault_authority.clone(),
                token_program.clone(),
                percolator_program.clone(),
            ],
        )?;
    }
    Ok(())
}

// init_insurance_pool accounts: [payer(s,w), mint, pool(w,pda), percolator_vault,
//   market_slab, percolator_program, system_program, vote_authority, coin_mint]
// data: asset_id (u64), policy (u8), optional deposit_window_slots (u64),
//       bootstrap_start_slot (u64), bootstrap_delay_slots (u64)
//
// `vote_authority` must be the canonical genesis-vote config PDA for (coin_mint,
// pool, bootstrap_delay_slots, bootstrap_start_slot). The pool commits to that
// same immutable schedule and requires the deposit window to close by bootstrap
// end, so permissionless init cannot split or invert the lifecycle.
//
// `percolator_vault` must be the canonical insurance vault token account for
// `market_slab` (the ATA of its vault_authority), owned by the vault_authority PDA.
#[inline(never)]
fn process_init_insurance_pool(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
    cross_backing: bool,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let vote_authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let backing_ledgers = if cross_backing {
        Some((next_account_info(iter)?, next_account_info(iter)?))
    } else {
        None
    };
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let asset_id = read_u64(data)?;
    let policy = read_u8(data)?;
    let (deposit_window_slots, deposit_start_slot, bootstrap_delay_slots) = if data.is_empty() {
        (
            DEFAULT_GENESIS_DEPOSIT_WINDOW_SLOTS,
            DEFAULT_GENESIS_DEPOSIT_START_SLOT,
            DEFAULT_GENESIS_BOOTSTRAP_DELAY_SLOTS,
        )
    } else {
        let window = read_u64(data)?;
        let start = read_u64(data)?;
        let delay = read_u64(data)?;
        if window == 0 || delay == 0 || !data.is_empty() {
            return Err(ProgramError::InvalidInstructionData);
        }
        (window, start, delay)
    };
    if policy > POLICY_WITH_SURPLUS || (cross_backing && policy != POLICY_PRINCIPAL) {
        return Err(ProgramError::InvalidInstructionData);
    }
    // This instruction is the genesis asset-0 insurance pool: deposits CPI to
    // Percolator TopUpInsurance (asset 0), while exits use an asset-indexed
    // withdraw. Nonzero asset IDs would make deposits and exits address different
    // insurance domains, potentially stranding depositors.
    if asset_id != 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if *percolator_program.key == Pubkey::default() {
        return Err(ProgramError::InvalidAccountData);
    }
    let now = Clock::get()?.slot;
    let deposit_deadline_slot = deposit_start_slot
        .checked_add(deposit_window_slots)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let bootstrap_end_slot = deposit_start_slot
        .checked_add(bootstrap_delay_slots)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if deposit_deadline_slot <= now
        || bootstrap_end_slot <= now
        || deposit_deadline_slot > bootstrap_end_slot
    {
        return Err(ProgramError::InvalidInstructionData);
    }

    let asset_id_bytes = asset_id.to_le_bytes();
    let policy_seed = [policy];
    let domain_seed = [DOMAIN_INSURANCE];
    let deposit_window_seed = deposit_window_slots.to_le_bytes();
    let deposit_start_seed = deposit_start_slot.to_le_bytes();
    let bootstrap_delay_seed = bootstrap_delay_slots.to_le_bytes();
    let full_pool_seeds = pool_seeds_full(
        mint.key,
        &asset_id_bytes,
        market_slab.key,
        percolator_program.key,
        coin_mint.key,
        &policy_seed,
        &domain_seed,
        &deposit_window_seed,
        &deposit_start_seed,
        &bootstrap_delay_seed,
    );
    let mut pool_pda_seeds = full_pool_seeds.to_vec();
    if cross_backing {
        pool_pda_seeds.push(CROSS_BACKING_POOL_SEED);
    }
    let (expected_pool, bump) = Pubkey::find_program_address(pool_pda_seeds.as_slice(), program_id);
    if *pool_account.key != expected_pool {
        return Err(ProgramError::InvalidSeeds);
    }
    if pool_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let expected_vote_authority = Pubkey::find_program_address(
        &[
            b"gv_config",
            coin_mint.key.as_ref(),
            pool_account.key.as_ref(),
            &bootstrap_delay_seed,
            &deposit_start_seed,
        ],
        &GENESIS_VOTE_PROGRAM_ID,
    )
    .0;
    if *vote_authority.key != expected_vote_authority {
        return Err(ProgramError::InvalidAccountData);
    }

    // The vault is the Percolator canonical insurance vault: an SPL token account
    // for `mint`, owned by the market's vault_authority PDA. Require SPL ownership before unpacking (see
    // init_pool above) — this path is genesis-atomic so a front-run is moot, but keep the guard for consistency
    // and any non-atomic reuse; percolator's CPI would also reject a non-SPL vault, this fails fast.
    let vault_authority = perc_vault_authority(market_slab.key, percolator_program.key);
    if percolator_vault.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let vault_state = spl_token::state::Account::unpack(&percolator_vault.try_borrow_data()?)?;
    if vault_state.mint != *mint.key || vault_state.owner != vault_authority {
        return Err(ProgramError::InvalidAccountData);
    }
    // Pin to the single canonical vault address Percolator enforces (F-VAULT-FRAG),
    // not merely "some vault_authority-owned token account". Binding a pool to a
    // non-canonical vault would leave it inert (every deposit/withdraw CPI reverts
    // with InvalidVaultAccount); reject it up front. Closes issue #24 on the
    // active path (PR #25 only covered the deprecated custodial program/).
    if *percolator_vault.key != canonical_vault_address(&vault_authority, mint.key) {
        return Err(ProgramError::InvalidAccountData);
    }

    let bump_arr = [bump];
    pool_pda_seeds.push(&bump_arr);
    let pool_size = POOL_SIZE_CUSTODY_GRANT;
    create_pda_robust(
        payer,
        pool_account,
        system_program,
        program_id,
        pool_pda_seeds.as_slice(),
        pool_size,
    )?;

    if let Some((long_ledger, short_ledger)) = backing_ledgers {
        if long_ledger.data_len() != 0
            || short_ledger.data_len() != 0
            || long_ledger.key == short_ledger.key
        {
            return Err(ProgramError::AccountAlreadyInitialized);
        }
        // Keep blank deterministic ledgers under Subledger ownership. The first
        // valid deposit transfers and binds both only after this pool is the
        // configured backing authority, leaving no public Percolator first write.
        for (domain, ledger) in [(0u16, long_ledger), (1u16, short_ledger)] {
            let domain_bytes = domain.to_le_bytes();
            let (expected, bump) = backing_ledger_pda(program_id, pool_account.key, domain);
            if *ledger.key != expected {
                return Err(ProgramError::InvalidSeeds);
            }
            let bump_seed = [bump];
            let ledger_seeds: [&[u8]; 4] = [
                BACKING_LEDGER_SEED,
                pool_account.key.as_ref(),
                &domain_bytes,
                &bump_seed,
            ];
            create_pda_robust_for_owner(
                payer,
                ledger,
                system_program,
                program_id,
                &ledger_seeds,
                percolator_accounting::backing_domain_ledger_account_len(),
            )?;
        }
    }

    let insurance_spent_checkpoint = if cross_backing {
        percolator_accounting::read_asset_insurance_spent(
            &market_slab.try_borrow_data()?,
            0,
        )
        .map_err(|_| ProgramError::InvalidAccountData)?
        .into_iter()
        .try_fold(0u128, |total, spent| total.checked_add(spent))
        .ok_or(ProgramError::ArithmeticOverflow)?
    } else {
        0
    };
    let pool = Pool {
        mint: *mint.key,
        asset_id,
        vault: *percolator_vault.key,
        outstanding_principal: 0,
        policy,
        domain: DOMAIN_INSURANCE,
        bump,
        share_generation: 0,
        market_slab: *market_slab.key,
        percolator_program: *percolator_program.key,
        vote_authority: *vote_authority.key,
        total_shares: 0,
        coin_mint: *coin_mint.key,
        deposit_deadline_slot,
        deposit_window_slots,
        deposit_start_slot,
        bootstrap_delay_slots,
        cross_backing,
        custody_granted: false,
        custody_grant_slot_plus_one: 0,
        pending_backing: [0, 0],
        share_rate_numerator: u128::from(cross_backing),
        share_rate_denominator: if cross_backing { VIRTUAL_SHARES } else { 0 },
        insurance_spent_checkpoint,
        backing_protected_checkpoint: 0,
    };
    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    Ok(())
}

// insurance_deposit accounts: [owner(s,w), pool(w), position(w,pda), owner_ata(w),
//   holding(w, pool-PDA-owned token acct), market_slab(w), percolator_vault(w),
//   [long_backing_ledger(w), short_backing_ledger(w) for cross-backing pools],
//   percolator_program, token_program, system_program]
// data: amount (u64)
//
// User -> holding (user-signed). Then the pool PDA (asset-0 insurance authority)
// tops up the two Percolator domains against the pool-wide 50/50 live protection target.
// Records the position (principal += amount, start_slot = now) and bumps outstanding.
#[inline(never)]
fn process_insurance_deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let owner_ata = next_account_info(iter)?;
    let holding = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    let backing_ledgers = if pool.cross_backing {
        Some([next_account_info(iter)?, next_account_info(iter)?])
    } else {
        None
    };
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if !pool.is_insurance() {
        return Err(ProgramError::InvalidAccountData);
    }
    // Historical genesis layouts either had no deposit deadline or did not bind
    // the complete bootstrap schedule into their PDA. Upgrades preserve exits,
    // never reopen those pools to late capital.
    if pool_account.data_len() < POOL_SIZE_CUSTODY_GRANT
        || !pool.custody_granted
        || (pool.cross_backing && pool_account.data_len() < POOL_SIZE_CROSS_BACKING)
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // Genesis deposits are only accepted during the configured window. Binding
    // the start slot into the pool PDA prevents a permissionless first writer
    // from opening deposits early or starting the clock before launch.
    let now = Clock::get()?.slot;
    let custody_grant_slot = pool
        .custody_grant_slot_plus_one
        .checked_sub(1)
        .ok_or(ProgramError::InvalidAccountData)?;
    if now <= custody_grant_slot
        || now < pool.deposit_start_slot
        || now >= pool.deposit_deadline_slot
    {
        return Err(ProgramError::InvalidInstructionData);
    }
    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    if *market_slab.key != pool.market_slab
        || *percolator_vault.key != pool.vault
        || *percolator_program.key != pool.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let backing_ledger_principals = if let Some(ledgers) = backing_ledgers {
        if percolator_accounting::read_asset_backing_authority(&market_slab.try_borrow_data()?, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != pool_account.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut principals = [0u128; 2];
        for (domain, ledger) in ledgers.into_iter().enumerate() {
            principals[domain] = bind_backing_ledger_if_needed(
                program_id,
                &pool,
                pool_seed_version,
                pool_account,
                market_slab,
                percolator_program,
                ledger,
                domain as u16,
            )?;
        }
        Some(principals)
    } else {
        None
    };
    // Cross-backed pools use one discoverable, clean pool ATA as both their
    // transfer transit and protocol-earnings escrow. Legacy pools preserve their
    // historical pool-owned holding behavior.
    let holding_balance_before = validate_insurance_holding(pool_account, &pool, holding)?;
    let pending_backing_before = pool.pending_backing_total()?;
    if holding_balance_before < pending_backing_before
        || pending_backing_before > pool.outstanding_principal
    {
        return Err(ProgramError::InvalidAccountData);
    }

    // Price shares before the top-up. WITH_SURPLUS uses the complete live balance so
    // pre-existing yield stays with earlier capital. PRINCIPAL excludes protocol surplus
    // and prices only the loss-bearing portion of the pool.
    let (
        insurance_balance_before,
        insurance_spent_before,
        backing_balance_before,
        backing_sources_before,
    ) = {
        let market_data = market_slab.try_borrow_data()?;
        let insurance = percolator_accounting::read_asset_insurance_balance(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let (spent, backing, sources) = if pool.cross_backing {
            (
                Some(
                    percolator_accounting::read_asset_insurance_spent(&market_data, 0)
                        .map_err(|_| ProgramError::InvalidAccountData)
                        .and_then(aggregate_insurance_spent)?,
                ),
                Some(
                    percolator_accounting::read_asset_backing_balances(&market_data, 0)
                        .map_err(|_| ProgramError::InvalidAccountData)?,
                ),
                Some(
                    percolator_accounting::read_asset_backing_source_credits(&market_data, 0)
                        .map_err(|_| ProgramError::InvalidAccountData)?,
                ),
            )
        } else {
            (None, None, None)
        };
        (insurance, spent, backing, sources)
    };
    let backing_protected_domains = match (
        backing_balance_before,
        backing_sources_before,
        backing_ledger_principals,
    ) {
        (Some(balances), Some(sources), Some(principals)) => [
            balances[0]
                .provider_protected_principal_atoms(principals[0], sources[0])
                .map_err(|_| ProgramError::InvalidAccountData)?,
            balances[1]
                .provider_protected_principal_atoms(principals[1], sources[1])
                .map_err(|_| ProgramError::InvalidAccountData)?,
        ],
        (None, None, None) => [0, 0],
        _ => return Err(ProgramError::InvalidAccountData),
    };
    let backing_effective_domains = [
        backing_protected_domains[0]
            .checked_add(u128::from(pool.pending_backing[0]))
            .ok_or(ProgramError::ArithmeticOverflow)?,
        backing_protected_domains[1]
            .checked_add(u128::from(pool.pending_backing[1]))
            .ok_or(ProgramError::ArithmeticOverflow)?,
    ];
    let backing_before = backing_effective_domains[0]
        .checked_add(backing_effective_domains[1])
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let owner_backing_before = owner_backing_protection(&pool, backing_before)?;
    let protected_before = insurance_balance_before
        .remaining_atoms
        .checked_add(backing_before)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let protected_before = u64::try_from(protected_before)
        .map_err(|_| ProgramError::ArithmeticOverflow)?;
    let indexed_cross_backing = uses_indexed_cross_backing(&pool, pool_account.data_len());
    if uses_external_loss_checkpoints(&pool, pool_account.data_len()) {
        sync_indexed_cross_backing_external_loss(
            &mut pool,
            protected_before,
            insurance_spent_before.ok_or(ProgramError::InvalidAccountData)?,
            owner_backing_before,
        )?;
    } else if indexed_cross_backing {
        sync_indexed_share_rate(&mut pool, protected_before)?;
    }
    let priced_balance_before = if pool.policy == POLICY_PRINCIPAL {
        core::cmp::min(protected_before, pool.outstanding_principal)
    } else {
        protected_before
    };
    let shares_minted = if indexed_cross_backing {
        begin_fully_impaired_indexed_recapitalization(&mut pool)?;
        let shares = mint_indexed_shares_with_capacity(&mut pool, amount)?;
        require_bounded_indexed_rounding(
            amount,
            shares,
            pool.share_rate_numerator,
            pool.share_rate_denominator,
        )?;
        shares
    } else {
        begin_fully_impaired_recapitalization(&mut pool, priced_balance_before)?;
        let (shares, virtual_shares) =
            mint_insurance_shares_with_capacity(&mut pool, amount, priced_balance_before)?;
        // Inflation/rounding guard (finding HB): a large surplus can make deposits mint zero or
        // very few shares. Reject before transfer unless their immediate value is within one atom.
        require_bounded_share_rounding_with_virtual_offset(
            amount,
            shares,
            pool.total_shares,
            priced_balance_before,
            virtual_shares,
        )?;
        shares
    };
    let outstanding_after = pool
        .outstanding_principal
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let protection_deposit = if pool.cross_backing {
        let before = percolator_accounting::balanced_insurance_domains(u128::from(
            pool.outstanding_principal,
        ));
        let after =
            percolator_accounting::balanced_insurance_domains(u128::from(outstanding_after));
        [
            after[0]
                .checked_sub(before[0])
                .ok_or(ProgramError::InvalidAccountData)?,
            after[1]
                .checked_sub(before[1])
                .ok_or(ProgramError::InvalidAccountData)?,
        ]
    } else {
        [u128::from(amount), 0]
    };
    let insurance_deposit = insurance_deposit_domain_delta(
        insurance_domain_balances(insurance_balance_before),
        u64::try_from(protection_deposit[0]).map_err(|_| ProgramError::ArithmeticOverflow)?,
    );
    let backing_deposit = insurance_deposit_domain_delta(
        backing_effective_domains,
        u64::try_from(protection_deposit[1]).map_err(|_| ProgramError::ArithmeticOverflow)?,
    );
    // Position PDA (one per owner per pool).
    let pos_seeds = position_seeds(pool_account.key, owner.key);
    let (expected_pos, pos_bump) = Pubkey::find_program_address(&pos_seeds, program_id);
    if *position_account.key != expected_pos {
        return Err(ProgramError::InvalidSeeds);
    }
    let mut position = if position_account.data_len() == 0 {
        let pbump = [pos_bump];
        let seeds: [&[u8]; 4] = [
            b"subledger_position",
            pool_account.key.as_ref(),
            owner.key.as_ref(),
            &pbump,
        ];
        create_pda_robust(
            owner,
            position_account,
            system_program,
            program_id,
            &seeds,
            POSITION_SIZE,
        )?;
        Position {
            pool: *pool_account.key,
            owner: *owner.key,
            principal: 0,
            withdrawn_amount: 0,
            withdrawn: false,
            start_slot: 0,
            vote_locked: false,
            terminal_returned: false,
            terminal_return_slot: None,
            share_generation: pool.share_generation,
            shares: 0,
        }
    } else {
        if position_account.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let p = Position::deserialize(&position_account.try_borrow_data()?)?;
        if position_account.data_len() < POSITION_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if p.owner != *owner.key || p.pool != *pool_account.key || p.withdrawn {
            return Err(ProgramError::InvalidAccountData);
        }
        p
    };
    move_position_to_current_share_generation(&mut position, &pool)?;

    // 1) User -> holding (user-signed; the user is moving their own funds).
    invoke(
        &spl_token::instruction::transfer(
            token_program.key,
            owner_ata.key,
            holding.key,
            owner.key,
            &[],
            amount,
        )?,
        &[
            owner_ata.clone(),
            holding.clone(),
            owner.clone(),
            token_program.clone(),
        ],
    )?;

    // 2) Split first across insurance/backing and then across long/short. Every
    // split uses live aggregate balances, so one-atom rounding is pool-wide rather
    // than selectable per depositor.
    for (domain, domain_amount) in insurance_deposit.into_iter().enumerate() {
        if domain_amount == 0 {
            continue;
        }
        let mut ix_data = vec![PERC_IX_TOP_UP_INSURANCE_DOMAIN];
        ix_data.extend_from_slice(&(domain as u16).to_le_bytes());
        ix_data.extend_from_slice(&domain_amount.to_le_bytes());
        invoke_signed_for_pool(
            &pool,
            pool_seed_version,
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*pool_account.key, true),
                    AccountMeta::new(*market_slab.key, false),
                    AccountMeta::new(*holding.key, false),
                    AccountMeta::new(*percolator_vault.key, false),
                    AccountMeta::new_readonly(*token_program.key, false),
                ],
                data: ix_data,
            },
            &[
                pool_account.clone(),
                market_slab.clone(),
                holding.clone(),
                percolator_vault.clone(),
                token_program.clone(),
                percolator_program.clone(),
            ],
        )?;
    }
    if let Some(ledgers) = backing_ledgers {
        // Percolator expiry forfeits fresh backing into junior residual; it is not
        // an unlock. Safe Genesis exit is enforced by this pool's vote,
        // owner-signature, valid-lien, and live-exposure gates instead.
        let expiry_slot = u64::MAX;
        let balances = backing_balance_before.ok_or(ProgramError::InvalidAccountData)?;
        for (domain, new_amount) in backing_deposit.into_iter().enumerate() {
            let staged_amount = pool.pending_backing[domain]
                .checked_add(
                    u64::try_from(new_amount).map_err(|_| ProgramError::ArithmeticOverflow)?,
                )
                .ok_or(ProgramError::ArithmeticOverflow)?;
            if staged_amount == 0 {
                continue;
            }
            if !balances[domain].accepts_top_up_expiry(expiry_slot) {
                pool.pending_backing[domain] = staged_amount;
                continue;
            }
            let domain_amount = u128::from(staged_amount);
            let mut ix_data = vec![PERC_IX_TOP_UP_BACKING_BUCKET];
            ix_data.extend_from_slice(&(domain as u16).to_le_bytes());
            ix_data.extend_from_slice(&domain_amount.to_le_bytes());
            ix_data.extend_from_slice(&expiry_slot.to_le_bytes());
            invoke_signed_for_pool(
                &pool,
                pool_seed_version,
                &Instruction {
                    program_id: *percolator_program.key,
                    accounts: vec![
                        AccountMeta::new_readonly(*pool_account.key, true),
                        AccountMeta::new(*market_slab.key, false),
                        AccountMeta::new(*holding.key, false),
                        AccountMeta::new(*percolator_vault.key, false),
                        AccountMeta::new_readonly(*token_program.key, false),
                        AccountMeta::new(*ledgers[domain].key, false),
                    ],
                    data: ix_data,
                },
                &[
                    pool_account.clone(),
                    market_slab.clone(),
                    holding.clone(),
                    percolator_vault.clone(),
                    token_program.clone(),
                    ledgers[domain].clone(),
                    percolator_program.clone(),
                ],
            )?;
            pool.pending_backing[domain] = 0;
        }
    }

    let pending_backing_after = pool.pending_backing_total()?;
    if pending_backing_after > outstanding_after {
        return Err(ProgramError::InvalidAccountData);
    }
    let expected_holding_after = holding_balance_before
        .checked_sub(pending_backing_before)
        .and_then(|escrow| escrow.checked_add(pending_backing_after))
        .ok_or(ProgramError::InvalidAccountData)?;
    if token_balance(holding)? != expected_holding_after {
        return Err(ProgramError::InvalidAccountData);
    }

    pool.outstanding_principal = outstanding_after;
    if uses_external_loss_checkpoints(&pool, pool_account.data_len()) {
        let backing_after = backing_before
            .checked_add(protection_deposit[1])
            .ok_or(ProgramError::ArithmeticOverflow)?;
        pool.backing_protected_checkpoint = owner_backing_protection(&pool, backing_after)?;
    }
    position.principal = position
        .principal
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    // Mint the priced shares (a top-up mints at the current price, accumulating onto the
    // position — its total shares represent its principal-weighted entry).
    pool.total_shares = pool
        .total_shares
        .checked_add(shares_minted)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    position.shares = position
        .shares
        .checked_add(shares_minted)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    // Last-write-time: topping up resets the vote clock.
    position.start_slot = Clock::get()?.slot;
    advance_position_nonce(&mut position);

    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

// insurance_withdraw accounts: [owner(s,w), pool(w), position(w), owner_ata(w),
//   holding(w, canonical pool ATA for cross-backed pools), market_slab(w),
//   percolator_vault(w), vault_authority,
//   [long_backing_ledger(w), short_backing_ledger(w) for cross-backing pools],
//   percolator_program, token_program]
// data: amount(u64) | expected_principal(u64) | expected_start_slot(u64) |
//   expected_action_nonce(u64)
//
// Owner-bound, principal-only exit: `amount <= position.principal`. The pool PDA
// (asset-0 insurance operator) signs WithdrawInsuranceAsset (tag 57). NOTE: the
// real percolator handler requires the withdraw destination to be owned by the
// *operator* (the pool PDA), not an arbitrary user, so we withdraw into a
// pool-PDA-owned holding account and then SPL-transfer holding -> owner's ATA
// (pool PDA signs). Can never exceed the owner's own recorded principal.
fn process_insurance_withdraw(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    let position_account = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    if position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let position = Position::deserialize(&position_account.try_borrow_data()?)?;
    let amount = read_u64(data)?;
    require_position_snapshot(data, &position)?;
    let amount_bytes = amount.to_le_bytes();
    let mut amount_data: &[u8] = &amount_bytes;
    process_insurance_withdraw_impl(program_id, accounts, &mut amount_data, false)
}

// insurance_withdraw_full has the same accounts as insurance_withdraw. Data is
// expected_principal(u64) | expected_start_slot(u64) | expected_action_nonce(u64),
// binding the owner signature to the exact position incarnation it removes. All
// owner, destination, pool, market, and payout checks stay centralized in the
// ordinary withdrawal implementation.
fn process_insurance_withdraw_full(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    let position_account = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    if position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let position = Position::deserialize(&position_account.try_borrow_data()?)?;
    require_position_snapshot(data, &position)?;
    let principal = position.principal;
    let amount_bytes = principal.to_le_bytes();
    let mut amount_data: &[u8] = &amount_bytes;
    process_insurance_withdraw_impl(program_id, accounts, &mut amount_data, false)
}

fn require_position_snapshot(data: &mut &[u8], position: &Position) -> ProgramResult {
    let expected_principal = read_u64(data)?;
    let expected_start_slot = read_u64(data)?;
    let expected_action_nonce = read_u64(data)?;
    if !data.is_empty()
        || expected_principal != position.principal
        || expected_start_slot != position.start_slot
        || expected_action_nonce != position.withdrawn_amount
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn advance_position_nonce(position: &mut Position) {
    // Historical binaries stored cumulative payout telemetry here and could
    // saturate it after a bounded number of large withdrawal cycles. Wrapping
    // preserves upgrade liveness; repeating a nonce within one valid signature
    // lifetime would require 2^64 successful position mutations.
    position.withdrawn_amount = position.withdrawn_amount.wrapping_add(1);
}

// return_finalized_position accounts: [owner, pool(w), position(w), owner_ata(w),
//   holding(w), market_slab(w), percolator_vault(w), vault_authority,
//   [long_backing_ledger(w), short_backing_ledger(w) for cross-backing pools],
//   percolator_program, token_program, genesis_config(w), genesis_ballot(w),
//   genesis_proposal(w), genesis_vote_program]
// data: none
//
// Once the real market is resolved and empty, anyone may retire an absent
// depositor's complete position after Genesis proves either a sealed distribution
// or expiry of its unsealed trigger phase. Genesis closes an unsealed election
// before atomically retiring the owner's exact live ballot, so refund order cannot
// choose a winner. The instruction has no amount and accepts only a clean token
// account owned by the depositor, so neither a cranker nor governance can capture
// or partially manipulate the payout.
fn process_return_finalized_position(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    process_insurance_withdraw_impl(program_id, accounts, data, true)
}

#[inline(never)]
fn process_insurance_withdraw_impl(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
    terminal: bool,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let owner = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let owner_ata = next_account_info(iter)?;
    let holding = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    let backing_ledgers = if pool.cross_backing {
        Some([next_account_info(iter)?, next_account_info(iter)?])
    } else {
        None
    };
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let genesis_accounts = if terminal {
        let config = next_account_info(iter)?;
        let ballot = next_account_info(iter)?;
        let proposal = next_account_info(iter)?;
        let genesis_program = next_account_info(iter)?;
        if iter.next().is_some() {
            return Err(ProgramError::InvalidInstructionData);
        }
        Some((config, ballot, proposal, genesis_program))
    } else {
        None
    };

    let requested_amount = if terminal {
        if !data.is_empty() {
            return Err(ProgramError::InvalidInstructionData);
        }
        None
    } else {
        let amount = read_u64(data)?;
        if amount == 0 || !data.is_empty() {
            return Err(ProgramError::InvalidInstructionData);
        }
        Some(amount)
    };
    if !terminal && !owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut position = Position::deserialize(&position_account.try_borrow_data()?)?;
    if !pool.is_insurance() {
        return Err(ProgramError::InvalidAccountData);
    }

    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    if *market_slab.key != pool.market_slab
        || *percolator_vault.key != pool.vault
        || *percolator_program.key != pool.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let backing_ledger_principals = if let Some(ledgers) = backing_ledgers {
        let mut principals = [0u128; 2];
        for (domain, ledger) in ledgers.into_iter().enumerate() {
            principals[domain] = validate_backing_ledger(
                program_id,
                pool_account.key,
                market_slab.key,
                percolator_program.key,
                ledger,
                domain as u16,
            )?;
        }
        if percolator_accounting::read_asset_backing_authority(
            &market_slab.try_borrow_data()?,
            0,
        )
        .map_err(|_| ProgramError::InvalidAccountData)?
            != pool_account.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Some(principals)
    } else {
        None
    };
    // vault_authority is a passed account, validated by PDA derivation.
    if *vault_authority.key != perc_vault_authority(market_slab.key, percolator_program.key) {
        return Err(ProgramError::InvalidSeeds);
    }
    // Positive payouts prove custody when Percolator accepts the pool-signed
    // withdrawal below. Zero-payout exits skip that CPI, so enforce the same
    // operator precondition before either path mutates pool accounting.
    if percolator_accounting::read_asset_insurance_operator(
        &market_slab.try_borrow_data()?,
        0,
    )
    .map_err(|_| ProgramError::InvalidAccountData)?
        != pool_account.key.to_bytes()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // The real Percolator handler requires the withdrawal destination to be owned
    // by the operator. For a cross-backed pool this is also the canonical protocol-
    // earnings escrow, whose starting balance must survive the owner's payout.
    let holding_balance_before = validate_insurance_holding(pool_account, &pool, holding)?;
    let pending_backing_before = pool.pending_backing_total()?;
    if holding_balance_before < pending_backing_before
        || pending_backing_before > pool.outstanding_principal
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // Position identity is always owner-bound. Ordinary exits additionally need
    // that owner's signature; terminal exits prove finality below.
    if position.owner != *owner.key || position.pool != *pool_account.key {
        return Err(ProgramError::IllegalOwner);
    }
    let expected_position =
        Pubkey::find_program_address(&position_seeds(pool_account.key, owner.key), program_id).0;
    if *position_account.key != expected_position {
        return Err(ProgramError::InvalidSeeds);
    }
    validate_owner_token_destination(owner_ata, owner.key, &pool.mint)?;
    if position.withdrawn {
        return Err(ProgramError::InvalidAccountData);
    }

    if terminal {
        let (genesis_config, genesis_ballot, genesis_proposal, genesis_program) =
            genesis_accounts.ok_or(ProgramError::NotEnoughAccountKeys)?;
        if pool_account.data_len() < POOL_SIZE
            || position_account.data_len() < POSITION_SIZE
            || pool.domain != DOMAIN_INSURANCE
            || pool.vote_authority != *genesis_config.key
            || *genesis_program.key != GENESIS_VOTE_PROGRAM_ID
            || !genesis_program.executable
        {
            return Err(ProgramError::InvalidAccountData);
        }
        // A cranker may create a fresh token account for an absent owner. Require
        // that owner and mint above, and reject authority-bearing destinations,
        // so a poisoned canonical ATA cannot preserve the cleanup DoS and the
        // cranker still cannot gain control of the payout.
        let owner_token = spl_token::state::Account::unpack(&owner_ata.try_borrow_data()?)?;
        if owner_token.delegate.is_some() || owner_token.close_authority.is_some() {
            return Err(ProgramError::InvalidAccountData);
        }
        {
            let market_data = market_slab.try_borrow_data()?;
            if !percolator_accounting::market_is_resolved_and_empty(&market_data)
                .map_err(|_| ProgramError::InvalidAccountData)?
            {
                return Err(ProgramError::InvalidAccountData);
            }
        }
        // The Genesis program attests the immutable deadline and canonical pool
        // binding, then retires this owner's exact live ballot. The pool PDA signer
        // prevents a direct caller from deleting votes. Market finality is proved
        // above, so custody recovery does not depend on a winning proposal.
        invoke_signed_for_pool(
            &pool,
            pool_seed_version,
            &Instruction {
                program_id: *genesis_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*pool_account.key, true),
                    AccountMeta::new(*genesis_config.key, false),
                    AccountMeta::new_readonly(*owner.key, false),
                    AccountMeta::new(*genesis_ballot.key, false),
                    AccountMeta::new(*genesis_proposal.key, false),
                ],
                data: vec![GENESIS_IX_RETIRE_TERMINAL_BALLOT],
            },
            &[
                pool_account.clone(),
                genesis_config.clone(),
                owner.clone(),
                genesis_ballot.clone(),
                genesis_proposal.clone(),
                genesis_program.clone(),
            ],
        )?;
    } else if position.vote_locked {
        // A live ballot must be retracted before its signing owner exits.
        return Err(ProgramError::InvalidAccountData);
    }

    let amount = requested_amount.unwrap_or(position.principal);
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    // Principal-only: never exceeds the owner's own recorded principal.
    if amount > position.principal || amount > pool.outstanding_principal {
        return Err(ProgramError::InsufficientFunds);
    }

    // Price current positions against every loss-bearing principal atom controlled
    // by this pool. Cross-backing earnings are protocol surplus and never increase
    // a genesis owner's claim.
    let (
        insurance,
        insurance_spent,
        live_insurance_balance,
        backing_balances,
        backing_sources,
        live_asset_exposed,
    ) = {
        let market_data = market_slab.try_borrow_data()?;
        let insurance = read_asset0_insurance(&market_data)?;
        let insurance_spent = if pool.cross_backing {
            percolator_accounting::read_asset_insurance_spent(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)
                .and_then(aggregate_insurance_spent)?
        } else {
            0
        };
        let live = percolator_accounting::market_is_live(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let live_balance = if live {
            Some(
                percolator_accounting::read_asset_insurance_balance(&market_data, 0)
                    .map_err(|_| ProgramError::InvalidAccountData)?,
            )
        } else {
            None
        };
        let exposed = live
            && percolator_accounting::asset_has_position_or_loss_state(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let (backing, sources) = if pool.cross_backing {
            (
                percolator_accounting::read_asset_backing_balances(&market_data, 0)
                    .map_err(|_| ProgramError::InvalidAccountData)?,
                percolator_accounting::read_asset_backing_source_credits(&market_data, 0)
                    .map_err(|_| ProgramError::InvalidAccountData)?,
            )
        } else {
            (
                [percolator_accounting::BackingDomainBalance::default(); 2],
                [percolator_accounting::BackingSourceCredit::default(); 2],
            )
        };
        (
            insurance,
            insurance_spent,
            live_balance,
            backing,
            sources,
            exposed,
        )
    };
    let backing_protected_domains = if let Some(principals) = backing_ledger_principals {
        [
            backing_balances[0]
                .provider_protected_principal_atoms(principals[0], backing_sources[0])
                .map_err(|_| ProgramError::InvalidAccountData)?,
            backing_balances[1]
                .provider_protected_principal_atoms(principals[1], backing_sources[1])
                .map_err(|_| ProgramError::InvalidAccountData)?,
        ]
    } else {
        [0, 0]
    };
    let backing_effective_domains = [
        backing_protected_domains[0]
            .checked_add(u128::from(pool.pending_backing[0]))
            .ok_or(ProgramError::ArithmeticOverflow)?,
        backing_protected_domains[1]
            .checked_add(u128::from(pool.pending_backing[1]))
            .ok_or(ProgramError::ArithmeticOverflow)?,
    ];
    let backing = backing_effective_domains[0]
        .checked_add(backing_effective_domains[1])
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let owner_backing = owner_backing_protection(&pool, backing)?;
    let protected_balance = u128::from(insurance)
        .checked_add(backing)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let indexed_cross_backing = uses_indexed_cross_backing(&pool, pool_account.data_len());
    if uses_external_loss_checkpoints(&pool, pool_account.data_len()) {
        sync_indexed_cross_backing_external_loss(
            &mut pool,
            protected_balance,
            insurance_spent,
            owner_backing,
        )?;
    } else if indexed_cross_backing {
        // Observe any market loss before touching this position's lazy share
        // generation. Deposits and prior exits can only leave physical surplus,
        // so this accumulator never rises from another owner's action.
        sync_indexed_share_rate(&mut pool, protected_balance)?;
    }
    let priced_balance = if pool.policy == POLICY_PRINCIPAL {
        core::cmp::min(protected_balance, pool.outstanding_principal)
    } else {
        protected_balance
    };
    // Burn the share fraction matching the withdrawn principal fraction for both policies.
    // WITH_SURPLUS redeems the live share value; PRINCIPAL uses it only as a loss cap
    // and never pays more than requested principal.
    let share_accounting = pool_account.data_len() >= POOL_SIZE_SHARES;
    if share_accounting && position_account.data_len() < POSITION_SIZE {
        return Err(ProgramError::InvalidAccountData);
    }
    let shares_are_current = if share_accounting {
        move_position_to_current_share_generation(&mut position, &pool)?
    } else {
        true
    };
    let virtual_shares = insurance_virtual_shares(pool.share_generation)?;
    let shares_to_retire = if !share_accounting || position.principal == 0 || !shares_are_current {
        0u128
    } else if amount == position.principal {
        position.shares
    } else {
        wide_mul_div_floor(position.shares, amount as u128, position.principal as u128)
            .ok_or(ProgramError::ArithmeticOverflow)?
    };
    let owed = if !shares_are_current {
        0
    } else if indexed_cross_backing && share_accounting {
        core::cmp::min(
            amount,
            redeem_indexed_shares(
                shares_to_retire,
                pool.share_rate_numerator,
                pool.share_rate_denominator,
            )?,
        )
    } else if pool.policy == POLICY_WITH_SURPLUS && share_accounting {
        redeem_shares_with_virtual_offset(
            shares_to_retire,
            priced_balance,
            pool.total_shares,
            virtual_shares,
        )?
    } else {
        if share_accounting && position.shares != 0 {
            if pool.total_shares == 0 {
                return Err(ProgramError::InvalidAccountData);
            }
            // Principal policy keeps all upside in the protocol, but loss follows
            // each position's priced entry. A deposit made after an impairment
            // therefore cannot recapitalize an older position at par.
            core::cmp::min(
                amount,
                redeem_shares_with_virtual_offset(
                    shares_to_retire,
                    priced_balance,
                    pool.total_shares,
                    virtual_shares,
                )?,
            )
        } else if !share_accounting {
            // Historical principal positions predate share accounting. Preserve
            // their owner-bound pro-rata exit instead of turning an upgrade into
            // a custody lock.
            payout(
                pool.policy,
                protected_balance,
                pool.outstanding_principal,
                amount,
            )?
        } else {
            // Current-layout positions can legitimately have zero shares after a
            // fully impaired generation reset or a lazy scale-down. They have no
            // claim on later recapitalization and must never enter the legacy path.
            0
        }
    };
    if owed > 0 && live_asset_exposed {
        return Err(ProgramError::InvalidAccountData);
    }
    let outstanding_after = pool
        .outstanding_principal
        .checked_sub(amount)
        .ok_or(ProgramError::InvalidAccountData)?;
    if pool.cross_backing
        && outstanding_after == 0
        && backing_balances
            .iter()
            .any(|balance| balance.valid_liened_principal_atoms != 0)
    {
        // The final owner action is the only principal-surplus cleanup for a
        // cross-backed pool. Keep it live until every valid lien is externally
        // released, then atomically collect the complete fresh remainder below.
        return Err(ProgramError::InvalidAccountData);
    }
    let final_cross_backing_sweep = pool.cross_backing
        && outstanding_after == 0
        && (pending_backing_before != 0
            || backing_balances.iter().any(|balance| {
                balance.principal_atoms != 0 || balance.earnings_atoms != 0
            }));
    if final_cross_backing_sweep && live_asset_exposed {
        return Err(ProgramError::InvalidAccountData);
    }
    // Debit the larger protection class first, reversing the aggregate deposit
    // tie-break. A haircut retires the unpaid nominal claim without moving state.
    let protection_debit = if pool.cross_backing {
        let before = percolator_accounting::balanced_insurance_domains(u128::from(
            pool.outstanding_principal,
        ));
        let after =
            percolator_accounting::balanced_insurance_domains(u128::from(outstanding_after));
        let nominal_debit = [
            before[0]
                .checked_sub(after[0])
                .ok_or(ProgramError::InvalidAccountData)?,
            before[1]
                .checked_sub(after[1])
                .ok_or(ProgramError::InvalidAccountData)?,
        ];
        protection_withdrawal_delta(
            [u128::from(insurance), backing],
            owed,
            nominal_debit,
        )?
    } else {
        [u128::from(owed), 0]
    };
    let insurance_owed = u64::try_from(protection_debit[0])
        .map_err(|_| ProgramError::ArithmeticOverflow)?;
    let backing_owed = u64::try_from(protection_debit[1])
        .map_err(|_| ProgramError::ArithmeticOverflow)?;
    let backing_claim_debit =
        insurance_withdraw_domain_delta(backing_effective_domains, backing_owed);
    let mut pending_backing_debit = [0u64; 2];
    let mut canonical_backing_debit = [0u128; 2];
    for domain in 0..2 {
        let pending = u128::from(pool.pending_backing[domain]);
        let staged_debit = core::cmp::min(pending, backing_claim_debit[domain]);
        pending_backing_debit[domain] =
            u64::try_from(staged_debit).map_err(|_| ProgramError::ArithmeticOverflow)?;
        canonical_backing_debit[domain] = backing_claim_debit[domain]
            .checked_sub(staged_debit)
            .ok_or(ProgramError::InvalidAccountData)?;
    }
    let pending_backing_debit_total = pending_backing_debit[0]
        .checked_add(pending_backing_debit[1])
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let canonical_backing_owed = backing_owed
        .checked_sub(pending_backing_debit_total)
        .ok_or(ProgramError::InvalidAccountData)?;
    let mut pending_backing_after = [
        pool.pending_backing[0]
            .checked_sub(pending_backing_debit[0])
            .ok_or(ProgramError::InvalidAccountData)?,
        pool.pending_backing[1]
            .checked_sub(pending_backing_debit[1])
            .ok_or(ProgramError::InvalidAccountData)?,
    ];
    if outstanding_after == 0 {
        // Any whole-atom remainder has no surviving owner claim. Keep the tokens
        // in escrow, but stop classifying them as protected principal.
        pending_backing_after = [0, 0];
    }
    let principal_debit = live_insurance_balance
        .map(|balance| {
            insurance_withdraw_domain_delta(
                insurance_domain_balances(balance),
                insurance_owed,
            )
        })
        .unwrap_or([0, 0]);
    let insurance_after = insurance
        .checked_sub(insurance_owed)
        .ok_or(ProgramError::InvalidAccountData)?;
    let backing_after = backing
        .checked_sub(u128::from(backing_owed))
        .ok_or(ProgramError::InvalidAccountData)?;
    let protected_after = u128::from(insurance_after)
        .checked_add(backing_after)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let priced_balance_after = if pool.policy == POLICY_PRINCIPAL {
        core::cmp::min(protected_after, outstanding_after)
    } else {
        protected_after
    };
    let pool_shares_to_burn = if shares_to_retire == 0 {
        0
    } else if indexed_cross_backing {
        // Retire exactly the withdrawing position's indexed shares. Its floored
        // token remainder stays protocol-owned in custody without entering any
        // surviving position's denominator or claim.
        shares_to_retire
    } else {
        rate_safe_pool_share_burn_with_virtual_offset(
            shares_to_retire,
            priced_balance,
            priced_balance_after,
            pool.total_shares,
            virtual_shares,
        )?
    };

    // The pool PDA (asset-0 insurance operator) signs WithdrawInsuranceAsset,
    // moving Percolator insurance -> pool-PDA-owned holding.
    // A fully-impaired exit normally retires without a zero-amount CPI. A final
    // cross-backed exit additionally isolates any unowned principal/earnings
    // remainder, even when the owner's own payout rounds to zero.
    if owed > 0 || final_cross_backing_sweep {
        // Once the final owner claim is retired, any fresh whole-atom share
        // remainder is protocol surplus. Isolate it in the same canonical escrow
        // as backing earnings; otherwise cross-backed pools have no authorized
        // whole-principal cleanup and one rounding atom can keep the market funded.
        let backing_debit = if pool.cross_backing && outstanding_after == 0 {
            backing_balances.map(|balance| balance.principal_atoms)
        } else {
            canonical_backing_debit
        };
        for (domain, debit) in backing_debit.into_iter().enumerate() {
            if debit > backing_balances[domain].principal_atoms {
                // Valid liens remain part of share value, but they are not liquid.
                // Wait for the externally cranked release instead of partially
                // mutating insurance and then discovering the backing lock.
                return Err(ProgramError::InvalidAccountData);
            }
        }
        // Percolator's backing bucket cannot transition to Empty while utilization
        // earnings remain. Sweep both exact live counters into the pool's canonical
        // escrow before any principal debit so an owner exit never depends on TWAP
        // having been configured. This transaction remains atomic on any later error.
        let backing_withdrawal = backing_debit
            .into_iter()
            .try_fold(0u128, |sum, amount| sum.checked_add(amount))
            .ok_or(ProgramError::ArithmeticOverflow)?;
        let protocol_backing_surplus = backing_withdrawal
            .checked_sub(u128::from(canonical_backing_owed))
            .ok_or(ProgramError::InvalidAccountData)?;
        let protocol_backing_surplus = u64::try_from(protocol_backing_surplus)
            .map_err(|_| ProgramError::ArithmeticOverflow)?;
        let swept_backing_earnings = if backing_withdrawal > 0 || final_cross_backing_sweep {
            let earnings = backing_balances.map(|balance| balance.earnings_atoms);
            let total = earnings
                .into_iter()
                .try_fold(0u128, |sum, amount| sum.checked_add(amount))
                .ok_or(ProgramError::ArithmeticOverflow)?;
            let total = u64::try_from(total).map_err(|_| ProgramError::ArithmeticOverflow)?;
            withdraw_cross_backing_earnings(
                &pool,
                pool_seed_version,
                pool_account,
                market_slab,
                holding,
                percolator_vault,
                vault_authority,
                backing_ledgers.ok_or(ProgramError::NotEnoughAccountKeys)?,
                percolator_program,
                token_program,
                earnings,
            )?;
            total
        } else {
            0
        };
        // In a live market, an asset-wide Percolator withdrawal debits the long domain first.
        // Withdrawing only `owed` would let a temporary depositor round-trip principal and move
        // another owner's protection into the short domain. Atomically withdraw far enough to
        // reverse this principal tranche's short debit and re-credit the excess long amount.
        // Any surplus payout follows the residual domain balances. Resolved markets
        // have no future side risk and reject live domain top-ups, so terminal returns
        // keep the direct path.
        let plan = if insurance_owed == 0 {
            None
        } else {
            live_insurance_balance
                .map(|balance| {
                    insurance_withdrawal_plan(
                        balance,
                        u128::from(insurance_owed),
                        principal_debit,
                    )
                })
                .transpose()?
        };
        if let Some(plan) = plan {
            if plan.redeposit != [0, 0]
                && percolator_accounting::read_asset_insurance_authority(
                    &market_slab.try_borrow_data()?,
                    0,
                )
                .map_err(|_| ProgramError::InvalidAccountData)?
                    != pool_account.key.to_bytes()
            {
                // The compensating domain top-up is authority-gated. Reject before
                // moving tokens if a predecessor layout split the insurance roles.
                return Err(ProgramError::InvalidAccountData);
            }
        }
        if insurance_owed > 0 {
            let gross_withdrawal = plan
                .map(|plan| plan.gross_withdrawal)
                .unwrap_or(u128::from(insurance_owed));
            u64::try_from(gross_withdrawal).map_err(|_| ProgramError::ArithmeticOverflow)?;
            let mut ix_data = vec![PERC_IX_WITHDRAW_INSURANCE_ASSET];
            // Genesis insurance deposits have always credited asset 0. Historical public
            // init accepted nonzero metadata IDs, but those remain PDA seed material only;
            // routing an exit by that stale value would strand the asset-0 principal.
            ix_data.extend_from_slice(&0u16.to_le_bytes());
            ix_data.extend_from_slice(&gross_withdrawal.to_le_bytes());
            invoke_signed_for_pool(
                &pool,
                pool_seed_version,
                &Instruction {
                    program_id: *percolator_program.key,
                    accounts: vec![
                        AccountMeta::new_readonly(*pool_account.key, true),
                        AccountMeta::new(*market_slab.key, false),
                        AccountMeta::new(*holding.key, false),
                        AccountMeta::new(*percolator_vault.key, false),
                        AccountMeta::new_readonly(*vault_authority.key, false),
                        AccountMeta::new_readonly(*token_program.key, false),
                    ],
                    data: ix_data,
                },
                &[
                    pool_account.clone(),
                    market_slab.clone(),
                    holding.clone(),
                    percolator_vault.clone(),
                    vault_authority.clone(),
                    token_program.clone(),
                    percolator_program.clone(),
                ],
            )?;

            if let Some(plan) = plan {
                for (domain, amount) in plan.redeposit.into_iter().enumerate() {
                    if amount == 0 {
                        continue;
                    }
                    let mut ix_data = vec![PERC_IX_TOP_UP_INSURANCE_DOMAIN];
                    ix_data.extend_from_slice(&(domain as u16).to_le_bytes());
                    ix_data.extend_from_slice(&amount.to_le_bytes());
                    invoke_signed_for_pool(
                        &pool,
                        pool_seed_version,
                        &Instruction {
                            program_id: *percolator_program.key,
                            accounts: vec![
                                AccountMeta::new_readonly(*pool_account.key, true),
                                AccountMeta::new(*market_slab.key, false),
                                AccountMeta::new(*holding.key, false),
                                AccountMeta::new(*percolator_vault.key, false),
                                AccountMeta::new_readonly(*token_program.key, false),
                            ],
                            data: ix_data,
                        },
                        &[
                            pool_account.clone(),
                            market_slab.clone(),
                            holding.clone(),
                            percolator_vault.clone(),
                            token_program.clone(),
                            percolator_program.clone(),
                        ],
                    )?;
                }
            }
        }

        if backing_withdrawal > 0 {
            let ledgers = backing_ledgers.ok_or(ProgramError::NotEnoughAccountKeys)?;
            let provider_principals =
                backing_ledger_principals.ok_or(ProgramError::NotEnoughAccountKeys)?;
            for (domain, amount) in backing_debit.into_iter().enumerate() {
                if amount == 0 {
                    continue;
                }
                // Funding and settlement can leave fresh whole atoms above this
                // provider's ledger principal. Debit only attributed principal
                // through the ledger; the excess is protocol surplus and uses
                // Percolator's ledgerless path. Both amounts are derived from
                // canonical state and move to the same pool-owned escrow.
                let provider_debit = core::cmp::min(amount, provider_principals[domain]);
                let protocol_debit = amount
                    .checked_sub(provider_debit)
                    .ok_or(ProgramError::InvalidAccountData)?;
                for (debit, ledger) in [
                    (provider_debit, Some(ledgers[domain])),
                    (protocol_debit, None),
                ] {
                    if debit == 0 {
                        continue;
                    }
                    let mut ix_data = vec![PERC_IX_WITHDRAW_BACKING_BUCKET];
                    ix_data.extend_from_slice(&(domain as u16).to_le_bytes());
                    ix_data.extend_from_slice(&debit.to_le_bytes());
                    let mut instruction_accounts = vec![
                        AccountMeta::new_readonly(*pool_account.key, true),
                        AccountMeta::new(*market_slab.key, false),
                        AccountMeta::new(*holding.key, false),
                        AccountMeta::new(*percolator_vault.key, false),
                        AccountMeta::new_readonly(*vault_authority.key, false),
                        AccountMeta::new_readonly(*token_program.key, false),
                    ];
                    let mut account_infos = vec![
                        pool_account.clone(),
                        market_slab.clone(),
                        holding.clone(),
                        percolator_vault.clone(),
                        vault_authority.clone(),
                        token_program.clone(),
                    ];
                    if let Some(ledger) = ledger {
                        instruction_accounts.push(AccountMeta::new(*ledger.key, false));
                        account_infos.push(ledger.clone());
                    }
                    account_infos.push(percolator_program.clone());
                    invoke_signed_for_pool(
                        &pool,
                        pool_seed_version,
                        &Instruction {
                            program_id: *percolator_program.key,
                            accounts: instruction_accounts,
                            data: ix_data,
                        },
                        &account_infos,
                    )?;
                }
            }
        }

        let escrow_after = holding_balance_before
            .checked_sub(pending_backing_debit_total)
            .and_then(|balance| balance.checked_add(swept_backing_earnings))
            .and_then(|balance| balance.checked_add(protocol_backing_surplus))
            .ok_or(ProgramError::InvalidAccountData)?;
        let holding_before_payout = escrow_after
            .checked_add(owed)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        if spl_token::state::Account::unpack(&holding.try_borrow_data()?)?.amount
            != holding_before_payout
        {
            return Err(ProgramError::InvalidAccountData);
        }
        if owed > 0 {
            // holding -> owner's ATA, signed by the pool PDA. The only path out,
            // bounded by the owner's pro-rata share, so the program can never pay
            // more than is owed. Protocol surplus remains in the canonical escrow.
            invoke_signed_for_pool(
                &pool,
                pool_seed_version,
                &spl_token::instruction::transfer(
                    token_program.key,
                    holding.key,
                    owner_ata.key,
                    pool_account.key,
                    &[],
                    owed,
                )?,
                &[
                    holding.clone(),
                    owner_ata.clone(),
                    pool_account.clone(),
                    token_program.clone(),
                ],
            )?;
        }
        if spl_token::state::Account::unpack(&holding.try_borrow_data()?)?.amount != escrow_after {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    // The full requested principal leaves the outstanding accounting (the loss, if any, is
    // realized); the owner collected `owed` (their pro-rata share).
    pool.pending_backing = pending_backing_after;
    pool.outstanding_principal = outstanding_after;
    if uses_external_loss_checkpoints(&pool, pool_account.data_len()) {
        pool.backing_protected_checkpoint = owner_backing_protection(&pool, backing_after)?;
    }
    position.principal -= amount;
    // The position retires its nominal shares. Historical pools burn only the
    // rate-safe subset and retain the difference as unowned reserve. Current
    // cross-backed pools burn the exact indexed amount while their rational rate
    // remains fixed. Empty principal pools reset for terminal custody.
    if shares_are_current {
        position.shares = position
            .shares
            .checked_sub(shares_to_retire)
            .ok_or(ProgramError::InvalidAccountData)?;
    } else if position.principal == 0 {
        position.shares = 0;
    }
    pool.total_shares = pool_total_shares_after_exit(
        pool.policy,
        outstanding_after,
        priced_balance_after,
        pool.total_shares,
        pool_shares_to_burn,
    )?;
    if outstanding_after == 0 {
        let (reset_generation, _) = share_generation_parts(pool.share_generation)?;
        pool.share_generation = encode_share_generation(reset_generation, 0)?;
        if indexed_cross_backing {
            pool.share_rate_numerator = 1;
            pool.share_rate_denominator = VIRTUAL_SHARES;
        }
    }
    // Historical telemetry must not become a custody gate. A position can cycle
    // the finite token supply enough times for cumulative withdrawals to exceed
    // u64 even though every individual balance and principal remains valid. A
    // permissionless terminal return instead records the remaining principal at
    // risk, preserving a frozen reward cap without restoring earlier withdrawals.
    if terminal {
        position.withdrawn_amount = amount;
    } else {
        advance_position_nonce(&mut position);
    }
    if position.principal == 0 {
        position.withdrawn = true;
        if terminal {
            // Terminal market finality ends the deposit's risk period; do not
            // leave a stale lock on retired capital. Genesis already removed the
            // ballot from its exact proposal and global tallies in this transaction.
            position.vote_locked = false;
            position.terminal_returned = true;
            position.terminal_return_slot = Some(core::cmp::min(
                Clock::get()?.slot,
                MAX_TERMINAL_RETURN_SLOT,
            ));
        }
    }

    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

// set_vote_lock accounts: [vote_authority(signer), pool, position(w), owner(signer)]
// data: locked (u8) — 1 lock, 0 unlock
//
// Toggles a position's vote-lock. ONLY the pool's registered vote_authority (the
// genesis-vote config PDA) may call it, and only on an insurance pool. This grants
// the genesis vote the right to BLOCK a withdrawal while a ballot is live — never
// to move funds. The owner always retains the ability to clear it by retracting
// their vote, so funds can never be permanently frozen by this mechanism.
fn process_set_vote_lock(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let vote_authority = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let position_account = next_account_info(iter)?;
    let owner = next_account_info(iter)?;

    let locked = read_u8(data)?;
    if locked > 1 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !vote_authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    // The position OWNER must also sign. Without this, an attacker who front-runs
    // pool init with an attacker-controlled vote_authority could lock any
    // depositor's position and freeze their withdrawal forever. Requiring the
    // owner's signature means a position can only ever be (un)locked in the context
    // of the owner acting on their OWN vote — which is the only legitimate case.
    // The vote_authority gate stays so the owner cannot self-unlock to bypass
    // retract (that would re-open the vote-outlives-capital hole).
    if !owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if pool_account.owner != program_id || position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    // Vote-locking is only meaningful for the insurance vote-bond pool, and only the
    // registered authority may toggle it. A default authority means locking is off.
    if !pool.is_insurance()
        || pool.vote_authority == Pubkey::default()
        || pool.vote_authority != *vote_authority.key
    {
        return Err(ProgramError::IllegalOwner);
    }
    let mut position = Position::deserialize(&position_account.try_borrow_data()?)?;
    if position.pool != *pool_account.key || position.owner != *owner.key {
        return Err(ProgramError::InvalidAccountData);
    }
    position.vote_locked = locked == 1;
    advance_position_nonce(&mut position);
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

fn validate_twap_recovery_grant(
    asset_admin: &AccountInfo,
    twap_config: &AccountInfo,
    pool_account: &AccountInfo,
    market_slab: &AccountInfo,
    percolator_program: &AccountInfo,
    require_loss_checkpoint: bool,
) -> Result<Option<(Option<u64>, Option<u128>)>, ProgramError> {
    if twap_config.owner != &TWAP_PROGRAM_ID {
        return Err(ProgramError::InvalidAccountData);
    }
    let expected_authority = Pubkey::find_program_address(
        &[TWAP_AUTHORITY_SEED, twap_config.key.as_ref()],
        &TWAP_PROGRAM_ID,
    )
    .0;
    if *asset_admin.key != expected_authority {
        return Err(ProgramError::InvalidAccountData);
    }
    let config_data = twap_config.try_borrow_data()?;
    if config_data.len() < TWAP_CUSTODY_CONFIG_MIN_SIZE
        || config_data[..8] != TWAP_CONFIG_DISC
        || config_data[40..72] != market_slab.key.to_bytes()
        || config_data[72..104] != percolator_program.key.to_bytes()
        || config_data[225..257] != pool_account.key.to_bytes()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if require_loss_checkpoint {
        // A 264-byte deployed config predates custody provenance. It still binds
        // the exact pool and had a working governance/terminal return path before
        // this upgrade, so recover it with the predecessor balance cap below.
        if config_data.len() == TWAP_CUSTODY_CONFIG_MIN_SIZE {
            return Ok(Some((None, None)));
        }
        if config_data.len() < TWAP_PROVENANCE_CONFIG_MIN_SIZE
            || config_data[TWAP_CUSTODY_MODE_OFF] != TWAP_CUSTODY_MODE_POOL_BOUND
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Some((
            Some(u64::from_le_bytes(
                config_data[TWAP_CUSTODY_PRINCIPAL_OFF..TWAP_CUSTODY_PRINCIPAL_OFF + 8]
                    .try_into()
                    .unwrap(),
            )),
            (config_data.len() >= TWAP_LOSS_CHECKPOINT_CONFIG_MIN_SIZE).then(|| {
                u128::from_le_bytes(
                    config_data[TWAP_INSURANCE_SPENT_OFF..TWAP_INSURANCE_SPENT_OFF + 16]
                        .try_into()
                        .unwrap(),
                )
            }),
        )))
    } else {
        Ok(None)
    }
}

// accept_operator accounts: [asset_admin(signer), pool(w), market_slab(w), percolator_program,
//   twap_config(optional; required after any user/provider value is admitted),
//   cross_backing_long_ledger?, cross_backing_short_ledger?]
// data: none
//
// The incoming pool PDA co-signs every rotation. Asset-admin moves LAST, after the
// insurance roles, so all three changes are atomic and the old externally controlled
// admin cannot recapture them after this instruction commits. This program exposes no
// arbitrary authority setter: the pool can sign only for this self-grant and the fixed
// TWAP handoff below.
#[inline(never)]
fn process_accept_operator(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let asset_admin = next_account_info(iter)?; // controller on first grant; TWAP on recovery
    let pool_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let twap_config = iter.next();
    let backing_ledgers = match iter.as_slice() {
        [] => None,
        [long, short] => Some([long, short]),
        _ => return Err(ProgramError::InvalidInstructionData),
    };

    if !asset_admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    if !pool.is_insurance() {
        return Err(ProgramError::InvalidAccountData);
    }
    if *market_slab.key != pool.market_slab || *percolator_program.key != pool.percolator_program {
        return Err(ProgramError::InvalidAccountData);
    }
    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    let first_grant = percolator_accounting::asset0_custody_can_be_first_granted(
        &market_slab.try_borrow_data()?,
    )
    .map_err(|_| ProgramError::InvalidAccountData)?;
    if first_grant
        && (!pool_account.is_writable
            || pool_account.data_len() < POOL_SIZE_CUSTODY_GRANT
            || pool.custody_granted)
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if !first_grant
        && pool_account.data_len() >= POOL_SIZE_CUSTODY_GRANT
        && !pool.custody_granted
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let custody_grant_slot_plus_one = if first_grant {
        Some(
            Clock::get()?
                .slot
                .checked_add(1)
                .ok_or(ProgramError::ArithmeticOverflow)?,
        )
    } else {
        None
    };
    if backing_ledgers.is_some() != (pool.cross_backing && twap_config.is_some()) {
        return Err(ProgramError::InvalidInstructionData);
    }
    let recovery_checkpoint = if let Some(twap_config) = twap_config {
        validate_twap_recovery_grant(
            asset_admin,
            twap_config,
            pool_account,
            market_slab,
            percolator_program,
            pool.cross_backing,
        )?
    } else if !first_grant {
        return Err(ProgramError::InvalidAccountData);
    } else {
        None
    };
    let persist_pool = if let Some((custody_principal, insurance_spent_checkpoint)) =
        recovery_checkpoint
    {
        if !pool_account.is_writable
            || !uses_indexed_cross_backing(&pool, pool_account.data_len())
            || custody_principal.is_some_and(|value| value > pool.outstanding_principal)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut provider_principals = [0u128; 2];
        for (domain, ledger) in backing_ledgers
            .ok_or(ProgramError::NotEnoughAccountKeys)?
            .into_iter()
            .enumerate()
        {
            provider_principals[domain] = validate_backing_ledger(
                program_id,
                pool_account.key,
                market_slab.key,
                percolator_program.key,
                ledger,
                domain as u16,
            )?;
        }
        let market_data = market_slab.try_borrow_data()?;
        if percolator_accounting::read_asset_backing_authority(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != pool_account.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let live_insurance = percolator_accounting::read_asset_insurance_remaining(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let insurance_spent =
            percolator_accounting::read_asset_insurance_spent(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)
                .and_then(aggregate_insurance_spent)?;
        let backing_balances =
            percolator_accounting::read_asset_backing_balances(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let backing_sources =
            percolator_accounting::read_asset_backing_source_credits(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let mut owner_backing = u128::from(pool.pending_backing_total()?);
        for domain in 0..2 {
            owner_backing = owner_backing
                .checked_add(
                    backing_balances[domain]
                        .provider_protected_principal_atoms(
                            provider_principals[domain],
                            backing_sources[domain],
                        )
                        .map_err(|_| ProgramError::InvalidAccountData)?,
                )
                .ok_or(ProgramError::ArithmeticOverflow)?;
        }
        let owner_backing = owner_backing_protection(&pool, owner_backing)?;
        let owner_insurance = if let Some(checkpoint) = insurance_spent_checkpoint {
            let insurance_loss = insurance_spent
                .checked_sub(checkpoint)
                .ok_or(ProgramError::InvalidAccountData)?;
            core::cmp::min(
                live_insurance,
                u128::from(custody_principal.ok_or(ProgramError::InvalidAccountData)?)
                    .saturating_sub(insurance_loss),
            )
        } else if let Some(custody_principal) = custody_principal {
            // A deployed 272-byte predecessor cannot persist a loss checkpoint.
            // Preserve its existing owner-recovery semantics instead of making an
            // upgrade strand custody. Every newly initialized config has a checkpoint.
            core::cmp::min(live_insurance, u128::from(custody_principal))
        } else {
            // A deployed 264-byte config records only the bound pool. Its old
            // recovery path priced owners from the live aggregate balance, so cap
            // insurance at the nominal complement and retain that behavior.
            core::cmp::min(
                live_insurance,
                u128::from(pool.outstanding_principal)
                    .saturating_sub(u128::from(owner_backing)),
            )
        };
        let protected = owner_insurance
            .checked_add(u128::from(owner_backing))
            .map(|value| value.min(u128::from(pool.outstanding_principal)))
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(ProgramError::ArithmeticOverflow)?;
        if uses_external_loss_checkpoints(&pool, pool_account.data_len()) {
            sync_indexed_cross_backing_external_loss(
                &mut pool,
                protected,
                insurance_spent,
                owner_backing,
            )?;
        } else {
            sync_indexed_share_rate(&mut pool, protected)?;
        }
        true
    } else {
        false
    };
    let rotate_backing = if pool.cross_backing {
        let market_data = market_slab.try_borrow_data()?;
        let authority = percolator_accounting::read_asset_backing_authority(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        if authority == pool_account.key.to_bytes() {
            false
        } else {
            let balances = percolator_accounting::read_asset_backing_balances(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
            if balances.into_iter().any(|balance| balance.has_any_state()) {
                return Err(ProgramError::InvalidAccountData);
            }
            true
        }
    } else {
        false
    };
    // Receive the two insurance roles, the empty backing role for current genesis
    // pools, and then asset_admin. The final rotation removes governance's direct
    // path to reassign any value-moving role.
    for kind in [
        ASSET_AUTH_INSURANCE,
        ASSET_AUTH_INSURANCE_OPERATOR,
        ASSET_AUTH_BACKING_BUCKET,
        ASSET_AUTH_ADMIN,
    ] {
        if kind == ASSET_AUTH_BACKING_BUCKET && !rotate_backing {
            continue;
        }
        let mut ix_data = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
        ix_data.extend_from_slice(&0u16.to_le_bytes()); // asset_index 0
        ix_data.push(kind);
        ix_data.extend_from_slice(pool_account.key.as_ref()); // new authority = the pool itself
        invoke_signed_for_pool(
            &pool,
            pool_seed_version,
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*asset_admin.key, true), // current asset_admin
                    AccountMeta::new_readonly(*pool_account.key, true), // new (the pool co-signs)
                    AccountMeta::new(*market_slab.key, false),
                ],
                data: ix_data,
            },
            &[
                asset_admin.clone(),
                pool_account.clone(),
                market_slab.clone(),
                percolator_program.clone(),
            ],
        )?;
    }
    if let Some(custody_grant_slot_plus_one) = custody_grant_slot_plus_one {
        pool.custody_granted = true;
        pool.custody_grant_slot_plus_one = custody_grant_slot_plus_one;
    }
    if persist_pool || custody_grant_slot_plus_one.is_some() {
        pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    }
    Ok(())
}

// return_resolved_asset0_backing accounts:
// [pool(current asset_admin), governance, controller, market(w), provider_ata(w),
//  controller_transit(w), percolator_vault(w), vault_authority,
//  long_backing_ledger(w), short_backing_ledger(w), percolator_program,
//  market_controller_program, token_program]
//
// Returning TWAP custody restores asset_admin to this market-bound insurance pool.
// Anyone may then crank the controller's fixed resolved cleanup, but this wrapper
// can sign only for the canonical pool PDA and exposes no amount or recipient.
fn process_return_resolved_asset0_backing(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let pool_account = next_account_info(iter)?;
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
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *controller_program.key != MARKET_CONTROLLER_PROGRAM_ID
        || !controller_program.executable
        || *token_program.key != spl_token::ID
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    // Historical insurance pools could carry a nonzero metadata asset_id even
    // though their public CPIs always funded asset 0. PDA validation plus the
    // immutable market/program/domain binding identifies them safely here.
    if !pool.is_insurance()
        || pool.cross_backing
        || pool.domain != DOMAIN_INSURANCE
        || *market_slab.key != pool.market_slab
        || *percolator_program.key != pool.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    invoke_signed_for_pool(
        &pool,
        pool_seed_version,
        &Instruction {
            program_id: *controller_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*governance.key, false),
                AccountMeta::new_readonly(*controller.key, false),
                AccountMeta::new_readonly(*pool_account.key, true),
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
            data: vec![CONTROLLER_IX_RETURN_RESOLVED_ASSET0_BACKING],
        },
        &[
            governance.clone(),
            controller.clone(),
            pool_account.clone(),
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
    )
}

// route_cross_backing_earnings accounts:
// [pool, twap_config, governance, market(w), pool_transit(w),
//  governance_destination(w), percolator_vault(w), vault_authority,
//  long_backing_ledger(w), short_backing_ledger(w), percolator_program,
//  twap_program, token_program]
//
// This is the only surplus path for a cross-backed genesis pool. It is
// intentionally amountless: the canonical pool escrow plus live Percolator
// earnings counters determine the complete transfer, and the fixed TWAP CPI
// constrains the recipient owner. Principal remains owner-bound.
fn process_route_cross_backing_earnings(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let pool_account = next_account_info(iter)?;
    let twap_config = next_account_info(iter)?;
    let governance = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let pool_transit = next_account_info(iter)?;
    let governance_destination = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let long_backing_ledger = next_account_info(iter)?;
    let short_backing_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let twap_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market_slab.is_writable
        || !pool_transit.is_writable
        || !governance_destination.is_writable
        || !percolator_vault.is_writable
        || !long_backing_ledger.is_writable
        || !short_backing_ledger.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *twap_program.key != TWAP_PROGRAM_ID
        || !twap_program.executable
        || *token_program.key != spl_token::ID
        || !percolator_program.executable
        || market_slab.owner != percolator_program.key
    {
        return Err(ProgramError::IncorrectProgramId);
    }

    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    if !pool.cross_backing
        || !pool.is_insurance()
        || pool.policy != POLICY_PRINCIPAL
        || pool.domain != DOMAIN_INSURANCE
        || *market_slab.key != pool.market_slab
        || *percolator_vault.key != pool.vault
        || *percolator_program.key != pool.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    for (domain, ledger) in [long_backing_ledger, short_backing_ledger]
        .into_iter()
        .enumerate()
    {
        validate_backing_ledger(
            program_id,
            pool_account.key,
            market_slab.key,
            percolator_program.key,
            ledger,
            domain as u16,
        )?;
    }
    let earnings = {
        let market_data = market_slab.try_borrow_data()?;
        if percolator_accounting::read_asset_backing_authority(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != pool_account.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        percolator_accounting::read_asset_backing_balances(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?
            .map(|balance| balance.earnings_atoms)
    };
    let live_earnings = earnings
        .into_iter()
        .try_fold(0u128, |total, amount| total.checked_add(amount))
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let live_earnings =
        u64::try_from(live_earnings).map_err(|_| ProgramError::ArithmeticOverflow)?;

    if *vault_authority.key != perc_vault_authority(market_slab.key, percolator_program.key)
        || percolator_vault.owner != &spl_token::ID
    {
        return Err(ProgramError::IllegalOwner);
    }
    let vault_state = spl_token::state::Account::unpack(&percolator_vault.try_borrow_data()?)?;
    if vault_state.state != spl_token::state::AccountState::Initialized
        || vault_state.owner != *vault_authority.key
        || vault_state.mint != pool.mint
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let escrow_before = validate_insurance_holding(pool_account, &pool, pool_transit)?;
    let pending_backing = pool.pending_backing_total()?;
    let routeable_escrow = escrow_before
        .checked_sub(pending_backing)
        .ok_or(ProgramError::InvalidAccountData)?;
    let total_earnings = routeable_escrow
        .checked_add(live_earnings)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if total_earnings == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    withdraw_cross_backing_earnings(
        &pool,
        pool_seed_version,
        pool_account,
        market_slab,
        pool_transit,
        percolator_vault,
        vault_authority,
        [long_backing_ledger, short_backing_ledger],
        percolator_program,
        token_program,
        earnings,
    )?;
    if spl_token::state::Account::unpack(&pool_transit.try_borrow_data()?)?.amount
        != pending_backing
            .checked_add(total_earnings)
            .ok_or(ProgramError::ArithmeticOverflow)?
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let mut twap_data = vec![TWAP_IX_ACCEPT_CROSS_BACKING_EARNINGS];
    twap_data.extend_from_slice(&total_earnings.to_le_bytes());
    twap_data.extend_from_slice(&pending_backing.to_le_bytes());
    invoke_signed_for_pool(
        &pool,
        pool_seed_version,
        &Instruction {
            program_id: *twap_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*pool_account.key, true),
                AccountMeta::new_readonly(*twap_config.key, false),
                AccountMeta::new_readonly(*governance.key, false),
                AccountMeta::new_readonly(*market_slab.key, false),
                AccountMeta::new(*pool_transit.key, false),
                AccountMeta::new(*governance_destination.key, false),
                AccountMeta::new_readonly(*percolator_program.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: twap_data,
        },
        &[
            pool_account.clone(),
            twap_config.clone(),
            governance.clone(),
            market_slab.clone(),
            pool_transit.clone(),
            governance_destination.clone(),
            percolator_program.clone(),
            token_program.clone(),
            twap_program.clone(),
        ],
    )?;
    if token_balance(pool_transit)? != pending_backing {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

// handoff_to_twap accounts:
// [squads_vault(signer on first handoff), pool(current asset_admin), twap_config,
//  twap_authority, market_slab(w), percolator_program,
//  cross_backing_long_ledger?, cross_backing_short_ledger?, twap_program]
// data: none
//
// Governance authorizes the first handoff but never receives a Percolator role. After
// an owner-bound recovery, the same current-layout TWAP config may accept the same pool
// again without governance; TWAP verifies that immutable binding before changing a
// role. The pool signs a CPI to the fixed TWAP program, which hardcodes the only
// incoming authority to its config-bound PDA and atomically protects this pool's live
// owner-insurance claim while canonical cross backing stays under the pool PDA.
// POLICY_WITH_SURPLUS may cross this boundary only after every owner claim is gone:
// while principal exists the live balance is depositor share value,
// but after the final exit any later fee or rounding reserve is protocol insurance that
// otherwise has no signer-backed terminal path.
// Percolator verifies that this pool is the current asset_admin, while the TWAP verifies
// the Squads identity and all market bindings.
#[inline(never)]
fn process_handoff_to_twap(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let pool_account = next_account_info(iter)?;
    let twap_config = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    let backing_ledgers = if pool.cross_backing {
        Some([next_account_info(iter)?, next_account_info(iter)?])
    } else {
        None
    };
    let twap_program = next_account_info(iter)?;

    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *twap_program.key != TWAP_PROGRAM_ID || !twap_program.executable {
        return Err(ProgramError::IncorrectProgramId);
    }
    if !pool.is_insurance()
        || (pool.policy == POLICY_WITH_SURPLUS && !pool.owner_claims_cleared())
        || *market_slab.key != pool.market_slab
        || *percolator_program.key != pool.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;

    let (protected_insurance_floor, insurance_spent_checkpoint) = if pool.cross_backing {
        let mut provider_principals = [0u128; 2];
        for (domain, ledger) in backing_ledgers
            .ok_or(ProgramError::NotEnoughAccountKeys)?
            .into_iter()
            .enumerate()
        {
            provider_principals[domain] = validate_backing_ledger(
                program_id,
                pool_account.key,
                market_slab.key,
                percolator_program.key,
                ledger,
                domain as u16,
            )?;
        }
        let market_data = market_slab.try_borrow_data()?;
        if percolator_accounting::read_asset_backing_authority(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != pool_account.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let backing_balances =
            percolator_accounting::read_asset_backing_balances(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let backing_sources =
            percolator_accounting::read_asset_backing_source_credits(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let mut backing = 0u128;
        for domain in 0..2 {
            backing = backing
                .checked_add(
                    backing_balances[domain]
                        .provider_protected_principal_atoms(
                            provider_principals[domain],
                            backing_sources[domain],
                        )
                        .map_err(|_| ProgramError::InvalidAccountData)?,
                )
                .ok_or(ProgramError::InvalidAccountData)?;
        }
        let pending_backing = pool.pending_backing_total()?;
        if pending_backing > pool.outstanding_principal {
            return Err(ProgramError::InvalidAccountData);
        }
        backing = backing
            .checked_add(u128::from(pending_backing))
            .ok_or(ProgramError::InvalidAccountData)?;
        let owner_backing = owner_backing_protection(&pool, backing)?;
        let insurance_spent =
            percolator_accounting::read_asset_insurance_spent(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)
                .and_then(aggregate_insurance_spent)?;
        let live_insurance =
            percolator_accounting::read_asset_insurance_remaining(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let protected_insurance = if uses_external_loss_checkpoints(
            &pool,
            pool_account.data_len(),
        ) {
            let physical = live_insurance
                .checked_add(u128::from(owner_backing))
                .map(|value| value.min(u128::from(pool.outstanding_principal)))
                .and_then(|value| u64::try_from(value).ok())
                .ok_or(ProgramError::ArithmeticOverflow)?;
            u128::from(indexed_protected_balance_after_external_loss(
                &pool,
                physical,
                insurance_spent,
                owner_backing,
            )?)
            .saturating_sub(u128::from(owner_backing))
            .min(live_insurance)
        } else {
            let nominal_insurance =
                u128::from(pool.outstanding_principal).saturating_sub(backing);
            core::cmp::min(nominal_insurance, live_insurance)
        };
        (
            protected_insurance,
            Some(insurance_spent),
        )
    } else {
        (u128::from(pool.outstanding_principal), None)
    };
    let mut handoff_data = vec![TWAP_IX_ACCEPT_FROM_SUBLEDGER];
    handoff_data.extend_from_slice(&protected_insurance_floor.to_le_bytes());
    if let Some(insurance_spent_checkpoint) = insurance_spent_checkpoint {
        handoff_data.extend_from_slice(&insurance_spent_checkpoint.to_le_bytes());
    }
    invoke_signed_for_pool(
        &pool,
        pool_seed_version,
        &Instruction {
            program_id: *twap_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*squads_vault.key, squads_vault.is_signer),
                AccountMeta::new_readonly(*pool_account.key, true),
                AccountMeta::new(*twap_config.key, false),
                AccountMeta::new_readonly(*twap_authority.key, false),
                AccountMeta::new(*market_slab.key, false),
                AccountMeta::new_readonly(*percolator_program.key, false),
            ],
            data: handoff_data,
        },
        &[
            squads_vault.clone(),
            pool_account.clone(),
            twap_config.clone(),
            twap_authority.clone(),
            market_slab.clone(),
            percolator_program.clone(),
            twap_program.clone(),
        ],
    )
}

// assert_no_principal accounts: [pool, optional holding_or_system]
// data: none
//
// This read-only CPI keeps the terminal owner-claim check in the program that
// owns the subledger layout. When a holding account is supplied, it also proves
// that holding is valid for the pool and empty; current cross-backed layouts bind
// it to the canonical protocol escrow. It moves no value and grants no authority.
fn process_assert_no_principal(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let pool_account = next_account_info(iter)?;
    let holding = iter.next();
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    validate_pool_pda(program_id, pool_account, &pool)?;
    if !pool.is_insurance() || !pool.owner_claims_cleared() {
        return Err(ProgramError::InvalidAccountData);
    }
    match holding {
        Some(system)
            if !pool.cross_backing
                && *system.key == solana_program::system_program::ID =>
        {}
        Some(holding) => {
            if validate_insurance_holding(pool_account, &pool, holding)? != 0 {
                return Err(ProgramError::InvalidAccountData);
            }
        }
        None => {}
    }
    Ok(())
}

// assert_principal accounts: [pool]
// data: none
//
// This is the positive counterpart to `assert_no_principal`. It proves a resolved
// TWAP custody return is restoring a real owner claim rather than moving protocol
// insurance into an empty pool.
fn process_assert_principal(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let pool_account = next_account_info(iter)?;
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    validate_pool_pda(program_id, pool_account, &pool)?;
    if !pool.is_insurance()
        || pool.policy != POLICY_PRINCIPAL
        || pool.outstanding_principal == 0
        || (pool_account.data_len() >= POOL_SIZE_SHARES && pool.total_shares == 0)
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for the pure payout arithmetic
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn insurance_balance(
        long_remaining: u128,
        long_withdrawable: u128,
        short_remaining: u128,
        short_withdrawable: u128,
        total_withdrawable: u128,
    ) -> percolator_accounting::InsuranceAssetBalance {
        percolator_accounting::InsuranceAssetBalance {
            domains: [
                percolator_accounting::InsuranceDomainBalance {
                    remaining_atoms: long_remaining,
                    withdrawable_atoms: long_withdrawable,
                },
                percolator_accounting::InsuranceDomainBalance {
                    remaining_atoms: short_remaining,
                    withdrawable_atoms: short_withdrawable,
                },
            ],
            remaining_atoms: long_remaining + short_remaining,
            withdrawable_atoms: total_withdrawable,
        }
    }

    #[test]
    fn live_insurance_exit_preserves_balanced_domains() {
        let plan = insurance_withdrawal_plan(
            insurance_balance(100, 100, 100, 100, 200),
            80,
            [40, 40],
        )
        .unwrap();
        assert_eq!(
            plan,
            InsuranceWithdrawalPlan {
                gross_withdrawal: 140,
                redeposit: [60, 0],
            }
        );
    }

    #[test]
    fn live_insurance_exit_reverses_its_principal_split_in_asymmetric_domains() {
        let plan = insurance_withdrawal_plan(
            insurance_balance(100, 100, 300, 300, 400),
            80,
            [40, 40],
        )
        .unwrap();
        assert_eq!(plan.gross_withdrawal, 140);
        assert_eq!(plan.redeposit, [60, 0]);
    }

    #[test]
    fn live_insurance_exit_respects_domain_reservation_floors() {
        let plan = insurance_withdrawal_plan(
            insurance_balance(100, 10, 100, 100, 110),
            80,
            [40, 40],
        )
        .unwrap();
        assert_eq!(plan.gross_withdrawal, 80);
        assert_eq!(plan.redeposit, [0, 0]);
        assert!(
            insurance_withdrawal_plan(
                insurance_balance(100, 10, 100, 100, 110),
                111,
                [56, 55],
            )
            .is_err()
        );
    }

    #[test]
    fn live_insurance_exit_honors_the_global_withdraw_capacity() {
        let plan = insurance_withdrawal_plan(
            insurance_balance(100, 100, 100, 100, 150),
            50,
            [25, 25],
        )
        .unwrap();
        assert_eq!(plan.gross_withdrawal, 125);
        assert_eq!(plan.redeposit, [75, 0]);
    }

    #[test]
    fn pool_wide_odd_atom_splits_reverse_for_every_small_round_trip() {
        for principal_before in 0..32u64 {
            for amount in 1..32u64 {
                let balances =
                    percolator_accounting::balanced_insurance_domains(principal_before.into());
                let deposit = insurance_deposit_domain_delta(balances, amount);
                let after_deposit = [
                    balances[0] + deposit[0],
                    balances[1] + deposit[1],
                ];
                let withdraw = insurance_withdraw_domain_delta(after_deposit, amount);
                assert_eq!(deposit, withdraw);
                assert_eq!(deposit[0] + deposit[1], u128::from(amount));
            }
        }
    }

    #[test]
    fn live_domain_routing_cannot_amplify_a_preexisting_skew() {
        for long in 0..32u128 {
            for short in 0..32u128 {
                let before_gap = long.abs_diff(short);
                for amount in 0..32u64 {
                    let deposit = insurance_deposit_domain_delta([long, short], amount);
                    assert_eq!(deposit[0] + deposit[1], u128::from(amount));
                    let after_deposit = [long + deposit[0], short + deposit[1]];
                    assert!(
                        after_deposit[0].abs_diff(after_deposit[1])
                            <= before_gap.saturating_sub(u128::from(amount)).max(1),
                    );

                    if u128::from(amount) <= long + short {
                        let debit = insurance_withdraw_domain_delta([long, short], amount);
                        assert_eq!(debit[0] + debit[1], u128::from(amount));
                        assert!(debit[0] <= long && debit[1] <= short);
                        let after_withdraw = [long - debit[0], short - debit[1]];
                        assert!(
                            after_withdraw[0].abs_diff(after_withdraw[1])
                                <= before_gap.saturating_sub(u128::from(amount)).max(1),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn impaired_odd_exit_reverses_only_principal_actually_paid() {
        for balances in [[100, 100], [100, 101]] {
            let fresh_deposit = insurance_deposit_domain_delta(balances, 1);
            let after_deposit = [
                balances[0] + fresh_deposit[0],
                balances[1] + fresh_deposit[1],
            ];

            // A stale odd claim retires three nominal atoms but receives only one.
            // Its net domain debit must reverse the fresh atom, not use the nominal
            // claim to choose a different side.
            let stale_exit = insurance_withdraw_domain_delta(after_deposit, 1);
            assert_eq!(stale_exit, fresh_deposit);

            // A fully impaired retirement moves no principal or live domain balance.
            assert_eq!(insurance_withdraw_domain_delta(after_deposit, 0), [0, 0]);
        }
    }

    #[test]
    fn impaired_payout_cannot_debit_beyond_its_nominal_domain_tranche() {
        for principal_before in 1..32u64 {
            for requested in 1..=principal_before {
                for payout in 1..=requested {
                    let balance = insurance_balance(1_000, 1_000, 1_000, 1_000, 2_000);
                    let principal_debit = insurance_withdraw_domain_delta(
                        insurance_domain_balances(balance),
                        payout,
                    );
                    let plan = insurance_withdrawal_plan(
                        balance,
                        u128::from(payout),
                        principal_debit,
                    )
                    .unwrap();
                    let gross_long_debit = core::cmp::min(
                        plan.gross_withdrawal,
                        balance.domains[0].withdrawable_atoms,
                    );
                    let gross_short_debit = plan.gross_withdrawal - gross_long_debit;
                    let long_debit = gross_long_debit - plan.redeposit[0];
                    let short_debit = gross_short_debit - plan.redeposit[1];
                    assert_eq!(long_debit + short_debit, u128::from(payout));
                    assert!(long_debit <= principal_debit[0]);
                    assert!(short_debit <= principal_debit[1]);
                }
            }
        }
    }

    #[test]
    fn with_surplus_exit_takes_principal_first_then_residual_yield() {
        let plan = insurance_withdrawal_plan(
            insurance_balance(150, 150, 250, 250, 400),
            120,
            [40, 40],
        )
        .unwrap();
        assert_eq!(plan.gross_withdrawal, 217);
        assert_eq!(plan.redeposit, [97, 0]);
    }

    // Authoritative pin for the exported POS_* offsets (finding HF follow-up): a Position serialized
    // with distinct field values must decode those values at exactly the published offsets. If the
    // layout ever shifts, this fails — and so does residual-distributor's cross-pin in offsets.rs.
    #[test]
    fn position_layout_offsets_match_serialize() {
        let pool = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let p = Position {
            pool,
            owner,
            principal: 0x1122_3344_5566_7788,
            withdrawn_amount: 0,
            withdrawn: true,
            start_slot: 0x0102_0304_0506_0708,
            vote_locked: false,
            terminal_returned: false,
            terminal_return_slot: None,
            share_generation: 3,
            shares: 0,
        };
        let mut d = vec![0u8; POSITION_SIZE];
        p.serialize(&mut d).unwrap();
        assert_eq!(&d[POS_POOL_OFF..POS_POOL_OFF + 32], pool.as_ref());
        assert_eq!(&d[POS_OWNER_OFF..POS_OWNER_OFF + 32], owner.as_ref());
        assert_eq!(
            u64::from_le_bytes(
                d[POS_PRINCIPAL_OFF..POS_PRINCIPAL_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            p.principal
        );
        assert_eq!(d[POS_WITHDRAWN_OFF], 1);
        assert_eq!(
            u64::from_le_bytes(
                d[POS_START_SLOT_OFF..POS_START_SLOT_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            p.start_slot
        );
        assert_eq!(
            decode_u40(&d[POS_SHARE_GENERATION_OFF..POS_SHARE_GENERATION_OFF + 5]),
            3,
        );

        let terminal = Position {
            pool,
            owner,
            principal: 0,
            withdrawn_amount: 77,
            withdrawn: true,
            start_slot: 55,
            vote_locked: false,
            terminal_returned: true,
            terminal_return_slot: Some(66),
            share_generation: 0,
            shares: 0,
        };
        terminal.serialize(&mut d).unwrap();
        assert_eq!(d[POS_TERMINAL_RETURNED_OFF], 1);
        let mut encoded_slot = [0u8; 8];
        encoded_slot[..5].copy_from_slice(
            &d[POS_TERMINAL_RETURN_SLOT_OFF..POS_TERMINAL_RETURN_SLOT_OFF + 5],
        );
        assert_eq!(u64::from_le_bytes(encoded_slot), 67);
        assert_eq!(
            u64::from_le_bytes(
                d[POS_WITHDRAWN_AMOUNT_OFF..POS_WITHDRAWN_AMOUNT_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            77
        );
        let decoded = Position::deserialize(&d).unwrap();
        assert!(decoded.terminal_returned);
        assert_eq!(decoded.terminal_return_slot, Some(66));
        d[POS_PRINCIPAL_OFF..POS_PRINCIPAL_OFF + 8].copy_from_slice(&1u64.to_le_bytes());
        assert!(Position::deserialize(&d).is_err());
    }

    #[test]
    fn principal_policy_healthy_pays_principal_keeps_surplus() {
        // balance 150 >= outstanding 100: each principal-100 exit gets exactly principal.
        assert_eq!(payout(POLICY_PRINCIPAL, 150, 100, 40).unwrap(), 40);
        assert_eq!(payout(POLICY_PRINCIPAL, 150, 100, 60).unwrap(), 60);
    }

    #[test]
    fn principal_policy_impaired_is_pro_rata() {
        // balance 50 < outstanding 100: pro-rata haircut.
        assert_eq!(payout(POLICY_PRINCIPAL, 50, 100, 40).unwrap(), 20);
        assert_eq!(payout(POLICY_PRINCIPAL, 50, 100, 60).unwrap(), 30);
    }

    #[test]
    fn with_surplus_returns_yield_pro_rata() {
        // balance 150, outstanding 100: surplus 50 distributed pro-rata.
        assert_eq!(payout(POLICY_WITH_SURPLUS, 150, 100, 40).unwrap(), 60);
        assert_eq!(payout(POLICY_WITH_SURPLUS, 150, 100, 60).unwrap(), 90);
    }

    #[test]
    fn rejects_degenerate_inputs() {
        assert!(payout(POLICY_PRINCIPAL, 100, 0, 10).is_err());
        assert!(payout(POLICY_PRINCIPAL, 100, 100, 0).is_err());
        assert!(payout(POLICY_PRINCIPAL, 100, 100, 101).is_err());
    }

    #[test]
    fn wide_mul_div_floor_is_exact_across_overflow_boundaries() {
        for a in [0, 1, 2, 17, u64::MAX as u128, u128::MAX / 2] {
            for b in [0, 1, 3, 19, u64::MAX as u128] {
                for denom in [1, 2, 7, u64::MAX as u128] {
                    if let Some(product) = a.checked_mul(b) {
                        assert_eq!(wide_mul_div_floor(a, b, denom), Some(product / denom));
                    }
                }
            }
        }

        let amount = 20_000_000_000_000_000u128;
        let shares = amount * VIRTUAL_SHARES;
        assert!(shares.checked_mul(amount + 1).is_none());
        assert_eq!(
            wide_mul_div_floor(shares, amount + 1, shares + VIRTUAL_SHARES),
            Some(amount)
        );
        assert_eq!(
            wide_mul_div_floor(amount, shares + VIRTUAL_SHARES, amount + 1,),
            Some(shares)
        );
        assert_eq!(
            wide_mul_div_floor(u128::MAX, u128::MAX, u128::MAX),
            Some(u128::MAX)
        );
        assert_eq!(wide_mul_div_floor(u128::MAX, u128::MAX, 1), None);
        assert_eq!(wide_mul_div_floor(1, 1, 0), None);
    }

    #[test]
    fn wide_mul_div_ceil_is_exact_across_overflow_boundaries() {
        for a in [0, 1, 2, 17, u64::MAX as u128, u128::MAX / 2] {
            for b in [0, 1, 3, 19, u64::MAX as u128] {
                for denom in [1, 2, 7, u64::MAX as u128] {
                    if let Some(product) = a.checked_mul(b) {
                        let expected = product / denom + u128::from(product % denom != 0);
                        assert_eq!(wide_mul_div_ceil(a, b, denom), Some(expected));
                    }
                }
            }
        }

        let amount = 20_000_000_000_000_000u128;
        let shares = amount * VIRTUAL_SHARES;
        assert!(shares.checked_mul(amount + 2).is_none());
        assert_eq!(
            wide_mul_div_ceil(shares, amount + 2, shares + VIRTUAL_SHARES),
            Some(amount + 1)
        );
        assert_eq!(wide_mul_div_ceil(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        assert_eq!(wide_mul_div_ceil(u128::MAX, u128::MAX, 1), None);
        assert_eq!(wide_mul_div_ceil(1, 1, 0), None);
    }

    #[test]
    fn insurance_share_rescale_preserves_residual_claim_and_deposit_capacity() {
        let mut pool = historical_pool_fixture();
        pool.domain = DOMAIN_INSURANCE;
        pool.policy = POLICY_PRINCIPAL;
        pool.outstanding_principal = u64::MAX;
        pool.share_generation = encode_share_generation(7, 0).unwrap();
        pool.total_shares = (u128::MAX / 4) * 3;
        let mut position = Position {
            pool: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            principal: u64::MAX,
            withdrawn_amount: 0,
            withdrawn: false,
            start_slot: 0,
            vote_locked: false,
            terminal_returned: false,
            terminal_return_slot: None,
            share_generation: pool.share_generation,
            shares: pool.total_shares,
        };
        let old_virtual = insurance_virtual_shares(pool.share_generation).unwrap();
        let old_claim = redeem_shares_with_virtual_offset(
            position.shares,
            1,
            pool.total_shares,
            old_virtual,
        )
        .unwrap();
        assert_eq!(old_claim, 1);

        let (minted, new_virtual) =
            mint_insurance_shares_with_capacity(&mut pool, 1, 1).unwrap();
        assert_eq!(share_generation_parts(pool.share_generation).unwrap(), (7, 1));
        move_position_to_current_share_generation(&mut position, &pool).unwrap();
        assert_eq!(
            redeem_shares_with_virtual_offset(
                position.shares,
                1,
                pool.total_shares,
                new_virtual,
            )
            .unwrap(),
            old_claim,
            "lazy scaling cannot write off the last claimable insurance atom",
        );
        require_bounded_share_rounding_with_virtual_offset(
            1,
            minted,
            pool.total_shares,
            1,
            new_virtual,
        )
        .unwrap();
        assert!(pool.total_shares.checked_add(minted).is_some());
    }

    #[test]
    fn total_impairment_invalidates_every_prior_scale_epoch() {
        let mut pool = historical_pool_fixture();
        pool.domain = DOMAIN_INSURANCE;
        pool.outstanding_principal = 1;
        pool.share_generation = encode_share_generation(11, 9).unwrap();
        pool.total_shares = 1u128 << 120;
        let mut position = Position {
            pool: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            principal: 1,
            withdrawn_amount: 0,
            withdrawn: false,
            start_slot: 0,
            vote_locked: false,
            terminal_returned: false,
            terminal_return_slot: None,
            share_generation: pool.share_generation,
            shares: 1u128 << 119,
        };

        begin_fully_impaired_recapitalization(&mut pool, 0).unwrap();
        assert_eq!(share_generation_parts(pool.share_generation).unwrap(), (12, 0));
        assert_eq!(pool.total_shares, 0);
        move_position_to_current_share_generation(&mut position, &pool).unwrap();
        assert_eq!(position.shares, 0);
    }

    #[test]
    fn lazy_share_scaling_never_increases_any_position_rate() {
        for scale_epoch in 0..32u64 {
            let old_generation = encode_share_generation(3, scale_epoch).unwrap();
            let new_generation = encode_share_generation(3, scale_epoch + 1).unwrap();
            let old_virtual = insurance_virtual_shares(old_generation).unwrap();
            let new_virtual = insurance_virtual_shares(new_generation).unwrap();
            for total in 2u128..=128 {
                let new_total = total.div_ceil(2);
                for shares in 0..=total {
                    let new_shares = shares / 2;
                    assert!(
                        new_shares * (total + old_virtual)
                            <= shares * (new_total + new_virtual),
                        "scale={scale_epoch}, total={total}, shares={shares}",
                    );
                    assert!(new_shares <= new_total);
                }
            }
        }
    }

    #[test]
    fn rate_safe_burn_reserves_floor_dust_instead_of_rewarding_last_exit() {
        let attacker_shares = mint_shares(3, 0, 0).unwrap();
        let victim_shares = mint_shares(1, attacker_shares, 3).unwrap();
        let total_shares = attacker_shares + victim_shares;

        let victim_payout = redeem_shares(victim_shares, 3, total_shares).unwrap();
        assert_eq!(victim_payout, 0);
        let victim_burn = rate_safe_pool_share_burn(
            victim_shares,
            3,
            3 - victim_payout,
            total_shares,
        )
        .unwrap();
        assert_eq!(victim_burn, 0, "a zero payout cannot burn another holder richer");

        let shares_after_victim = total_shares - victim_burn;
        let attacker_payout = redeem_shares(attacker_shares, 3, shares_after_victim).unwrap();
        assert_eq!(attacker_payout, 2);
        assert_eq!(3 - victim_payout - attacker_payout, 1);
    }

    #[test]
    fn rate_safe_burn_retires_all_shares_for_an_exact_healthy_redemption() {
        let shares = mint_shares(7, 0, 0).unwrap();
        let payout = redeem_shares(shares, 7, shares).unwrap();
        assert_eq!(payout, 7);
        assert_eq!(
            rate_safe_pool_share_burn(shares, 7, 7 - payout, shares).unwrap(),
            shares
        );
    }

    #[test]
    fn rate_safe_burn_is_the_maximal_non_appreciating_burn() {
        for total_units in 1u128..=16 {
            let total_shares = total_units * VIRTUAL_SHARES;
            for retired_units in 0..=total_units {
                let shares_retired = retired_units * VIRTUAL_SHARES;
                for balance_before in 0u64..=32 {
                    for balance_after in 0..=balance_before {
                        let burned = rate_safe_pool_share_burn(
                            shares_retired,
                            balance_before,
                            balance_after,
                            total_shares,
                        )
                        .unwrap();
                        assert!(burned <= shares_retired);

                        let numerator_before = balance_before as u128 + 1;
                        let numerator_after = balance_after as u128 + 1;
                        let denominator_before = total_shares + VIRTUAL_SHARES;
                        let denominator_after = denominator_before - burned;
                        assert!(
                            numerator_after * denominator_before
                                <= numerator_before * denominator_after,
                            "remaining shares appreciated: before={balance_before}, after={balance_after}, total={total_shares}, retired={shares_retired}, burned={burned}",
                        );

                        if burned < shares_retired {
                            assert!(
                                numerator_after * denominator_before
                                    > numerator_before * (denominator_after - 1),
                                "one more share could have been burned safely",
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn indexed_principal_split_exits_preserve_surviving_claims() {
        #[derive(Clone, Copy, Debug)]
        struct ExitState {
            protected: u64,
            outstanding: u64,
            total_shares: u128,
            share_rate_numerator: u128,
            share_rate_denominator: u128,
            attacker_principal: u64,
            attacker_shares: u128,
            victim_principal: u64,
            victim_shares: u128,
        }

        fn exit(mut state: ExitState, attacker: bool, amount: u64) -> (ExitState, u64) {
            let (principal, shares) = if attacker {
                (state.attacker_principal, state.attacker_shares)
            } else {
                (state.victim_principal, state.victim_shares)
            };
            let shares_retired = if amount == principal {
                shares
            } else {
                wide_mul_div_floor(shares, amount as u128, principal as u128).unwrap()
            };
            let paid = core::cmp::min(
                amount,
                redeem_indexed_shares(
                    shares_retired,
                    state.share_rate_numerator,
                    state.share_rate_denominator,
                )
                .unwrap(),
            );
            let outstanding_after = state.outstanding - amount;
            let protected_after = state.protected - paid;
            state.total_shares -= shares_retired;
            state.protected = protected_after;
            state.outstanding = outstanding_after;
            if attacker {
                state.attacker_principal -= amount;
                state.attacker_shares -= shares_retired;
            } else {
                state.victim_principal -= amount;
                state.victim_shares -= shares_retired;
            }
            if outstanding_after == 0 {
                state.attacker_shares = 0;
                state.victim_shares = 0;
            }
            assert!(
                state.total_shares >= state.attacker_shares + state.victim_shares,
                "an exit burned surviving owned shares: {state:?}",
            );
            (state, paid)
        }

        for attacker_principal in 1u64..=16 {
            for victim_principal in 1u64..=16 {
                let outstanding = attacker_principal + victim_principal;
                for protected in 0..=outstanding {
                    let mut pool = historical_pool_fixture();
                    pool.cross_backing = true;
                    pool.policy = POLICY_PRINCIPAL;
                    pool.domain = DOMAIN_INSURANCE;
                    pool.outstanding_principal = outstanding;
                    pool.total_shares = u128::from(outstanding) * VIRTUAL_SHARES;
                    pool.share_rate_numerator = 1;
                    pool.share_rate_denominator = VIRTUAL_SHARES;
                    sync_indexed_share_rate(&mut pool, protected).unwrap();
                    let initial = ExitState {
                        protected,
                        outstanding,
                        total_shares: pool.total_shares,
                        share_rate_numerator: pool.share_rate_numerator,
                        share_rate_denominator: pool.share_rate_denominator,
                        attacker_principal,
                        attacker_shares: u128::from(attacker_principal) * VIRTUAL_SHARES,
                        victim_principal,
                        victim_shares: u128::from(victim_principal) * VIRTUAL_SHARES,
                    };
                    let initial_attacker_claim = core::cmp::min(
                        attacker_principal,
                        redeem_indexed_shares(
                            initial.attacker_shares,
                            initial.share_rate_numerator,
                            initial.share_rate_denominator,
                        )
                        .unwrap(),
                    );
                    let initial_victim_claim = core::cmp::min(
                        victim_principal,
                        redeem_indexed_shares(
                            initial.victim_shares,
                            initial.share_rate_numerator,
                            initial.share_rate_denominator,
                        )
                        .unwrap(),
                    );
                    let (control, control_attacker_paid) =
                        exit(initial, true, attacker_principal);
                    let (_, control_victim_paid) = exit(control, false, victim_principal);
                    assert!(control_attacker_paid <= initial_attacker_claim);
                    assert!(control_victim_paid >= initial_victim_claim);

                    for chunk in 1..=attacker_principal {
                        let mut split = initial;
                        let mut split_attacker_paid = 0u64;
                        while split.attacker_principal != 0 {
                            let amount = core::cmp::min(chunk, split.attacker_principal);
                            let (next, paid) = exit(split, true, amount);
                            split = next;
                            split_attacker_paid += paid;
                        }
                        let (_, split_victim_paid) = exit(split, false, victim_principal);
                        assert!(
                            split_attacker_paid <= initial_attacker_claim,
                            "splitter exceeded its pre-sequence claim: initial={initial:?}, chunk={chunk}, claim={initial_attacker_claim}, split={split_attacker_paid}",
                        );
                        assert!(
                            split_victim_paid >= initial_victim_claim,
                            "splitter reduced victim below its pre-sequence claim: initial={initial:?}, chunk={chunk}, claim={initial_victim_claim}, split={split_victim_paid}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn indexed_rate_moves_only_for_external_loss() {
        let mut pool = historical_pool_fixture();
        pool.cross_backing = true;
        pool.policy = POLICY_PRINCIPAL;
        pool.domain = DOMAIN_INSURANCE;
        pool.outstanding_principal = 28;
        pool.total_shares = 28 * VIRTUAL_SHARES;
        pool.share_rate_numerator = 1;
        pool.share_rate_denominator = VIRTUAL_SHARES;

        sync_indexed_share_rate(&mut pool, 26).unwrap();
        let impaired_rate = (pool.share_rate_numerator, pool.share_rate_denominator);
        assert_eq!(
            compare_fractions(impaired_rate.0, impaired_rate.1, 1, VIRTUAL_SHARES).unwrap(),
            core::cmp::Ordering::Less,
        );
        assert_eq!(
            redeem_indexed_shares(14 * VIRTUAL_SHARES, impaired_rate.0, impaired_rate.1).unwrap(),
            13,
        );

        sync_indexed_share_rate(&mut pool, 28).unwrap();
        assert_eq!(
            (pool.share_rate_numerator, pool.share_rate_denominator),
            impaired_rate,
            "surplus cannot raise claims",
        );

        pool.total_shares -= 14 * VIRTUAL_SHARES;
        sync_indexed_share_rate(&mut pool, 13).unwrap();
        assert_eq!(
            (pool.share_rate_numerator, pool.share_rate_denominator),
            impaired_rate,
            "an exit cannot lower claims",
        );
        assert_eq!(
            redeem_indexed_shares(
                14 * VIRTUAL_SHARES,
                pool.share_rate_numerator,
                pool.share_rate_denominator,
            )
            .unwrap(),
            13,
        );

        let mut exact_threshold = historical_pool_fixture();
        exact_threshold.cross_backing = true;
        exact_threshold.policy = POLICY_PRINCIPAL;
        exact_threshold.domain = DOMAIN_INSURANCE;
        exact_threshold.outstanding_principal = 5;
        exact_threshold.total_shares = 5 * VIRTUAL_SHARES;
        exact_threshold.share_rate_numerator = 1;
        exact_threshold.share_rate_denominator = VIRTUAL_SHARES;
        sync_indexed_share_rate(&mut exact_threshold, 1).unwrap();
        assert_eq!(
            redeem_indexed_shares(
                3 * VIRTUAL_SHARES,
                exact_threshold.share_rate_numerator,
                exact_threshold.share_rate_denominator,
            )
            .unwrap(),
            1,
            "the exact rational rate cannot round an existing whole-atom claim away",
        );
    }

    #[test]
    fn indexed_external_loss_checkpoints_apply_each_delta_once() {
        let mut pool = historical_pool_fixture();
        pool.cross_backing = true;
        pool.policy = POLICY_PRINCIPAL;
        pool.domain = DOMAIN_INSURANCE;
        pool.outstanding_principal = 29;
        pool.total_shares = 29 * VIRTUAL_SHARES;
        pool.share_rate_numerator = 1;
        pool.share_rate_denominator = VIRTUAL_SHARES;
        pool.insurance_spent_checkpoint = 10;
        pool.backing_protected_checkpoint = 15;

        sync_indexed_cross_backing_external_loss(&mut pool, 40, 12, 14).unwrap();
        assert_eq!(pool.insurance_spent_checkpoint, 12);
        assert_eq!(pool.backing_protected_checkpoint, 14);
        assert_eq!(
            redeem_indexed_shares(
                pool.total_shares,
                pool.share_rate_numerator,
                pool.share_rate_denominator,
            )
            .unwrap(),
            26,
            "insurance consumption and canonical backing loss are both applied",
        );
        let impaired_rate = (pool.share_rate_numerator, pool.share_rate_denominator);

        pool.total_shares -= VIRTUAL_SHARES;
        pool.outstanding_principal -= 1;
        sync_indexed_cross_backing_external_loss(&mut pool, 40, 12, 14).unwrap();
        assert_eq!(
            (pool.share_rate_numerator, pool.share_rate_denominator),
            impaired_rate,
            "a zero-value share exit cannot feed an aggregate floor back into the rate",
        );

        sync_indexed_cross_backing_external_loss(&mut pool, 40, 12, 15).unwrap();
        assert_eq!(
            (pool.share_rate_numerator, pool.share_rate_denominator),
            impaired_rate,
            "a fee buffer or backing recovery cannot raise the indexed owner loss",
        );
        assert!(sync_indexed_cross_backing_external_loss(&mut pool, 40, 11, 15).is_err());
        assert_eq!(pool.insurance_spent_checkpoint, 12);
        assert_eq!(pool.backing_protected_checkpoint, 15);

        sync_indexed_cross_backing_external_loss(&mut pool, 40, 13, 14).unwrap();
        assert_eq!(
            redeem_indexed_shares(
                pool.total_shares,
                pool.share_rate_numerator,
                pool.share_rate_denominator,
            )
            .unwrap(),
            23,
            "only newly observed insurance and backing loss lower the surviving claim",
        );
    }

    #[test]
    fn fraction_comparison_is_exact_without_cross_product_overflow() {
        for left_numerator in 0u128..=32 {
            for left_denominator in 1u128..=32 {
                for right_numerator in 0u128..=32 {
                    for right_denominator in 1u128..=32 {
                        assert_eq!(
                            compare_fractions(
                                left_numerator,
                                left_denominator,
                                right_numerator,
                                right_denominator,
                            )
                            .unwrap(),
                            (left_numerator * right_denominator)
                                .cmp(&(right_numerator * left_denominator)),
                        );
                    }
                }
            }
        }
        assert_eq!(
            compare_fractions(u128::MAX, u128::MAX - 1, u128::MAX - 1, u128::MAX)
                .unwrap(),
            core::cmp::Ordering::Greater,
        );
    }

    #[test]
    fn empty_with_surplus_epoch_normalizes_reserve_for_minimum_reentry() {
        let reserve_balance = 2u64;
        let reserve_shares = pool_total_shares_after_exit(
            POLICY_WITH_SURPLUS,
            0,
            reserve_balance,
            1_006_623,
            0,
        )
        .unwrap();
        assert_eq!(reserve_shares, reserve_balance as u128 * VIRTUAL_SHARES);
        let new_shares = mint_shares(1, reserve_shares, reserve_balance).unwrap();
        require_bounded_share_rounding(1, new_shares, reserve_shares, reserve_balance).unwrap();
        assert_eq!(new_shares, VIRTUAL_SHARES);
        assert_eq!(redeem_shares(new_shares, 3, reserve_shares + new_shares).unwrap(), 1);

        assert_eq!(
            pool_total_shares_after_exit(POLICY_PRINCIPAL, 0, reserve_balance, 9_999, 1)
                .unwrap(),
            0,
            "principal pools retain their zero-claim terminal attestation",
        );
    }

    #[test]
    fn state_round_trips() {
        let slab = Pubkey::new_unique();
        let perc = Pubkey::new_unique();
        let coin_mint = Pubkey::new_unique();
        let pool = Pool {
            mint: Pubkey::new_unique(),
            asset_id: 7,
            vault: Pubkey::new_unique(),
            outstanding_principal: 12345,
            policy: POLICY_WITH_SURPLUS,
            domain: DOMAIN_BACKING,
            bump: 254,
            share_generation: 7,
            market_slab: slab,
            percolator_program: perc,
            vote_authority: Pubkey::new_unique(),
            total_shares: 7_777,
            coin_mint,
            deposit_deadline_slot: 42_424,
            deposit_window_slots: 99,
            deposit_start_slot: 12_345,
            bootstrap_delay_slots: 30_000,
            cross_backing: false,
            custody_granted: false,
            custody_grant_slot_plus_one: 0,
            pending_backing: [0, 0],
            share_rate_numerator: 0,
            share_rate_denominator: 0,
            insurance_spent_checkpoint: 0,
            backing_protected_checkpoint: 0,
        };
        let mut buf = [0u8; POOL_SIZE];
        pool.serialize(&mut buf).unwrap();
        // Canary the exported quorum-denominator offset (finding ID) so consumers can cross-pin it.
        assert_eq!(
            u64::from_le_bytes(
                buf[POOL_OUTSTANDING_PRINCIPAL_OFF..POOL_OUTSTANDING_PRINCIPAL_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            12345,
            "Pool.outstanding_principal must serialize at POOL_OUTSTANDING_PRINCIPAL_OFF"
        );
        assert_eq!(
            u64::from_le_bytes(
                buf[POOL_DEPOSIT_DEADLINE_SLOT_OFF..POOL_DEPOSIT_DEADLINE_SLOT_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            42_424
        );
        assert_eq!(
            u64::from_le_bytes(
                buf[POOL_DEPOSIT_WINDOW_SLOTS_OFF..POOL_DEPOSIT_WINDOW_SLOTS_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            99
        );
        let d = Pool::deserialize(&buf).unwrap();
        assert_eq!(d.mint, pool.mint);
        assert_eq!(d.asset_id, 7);
        assert_eq!(d.vault, pool.vault);
        assert_eq!(d.outstanding_principal, 12345);
        assert_eq!(d.policy, POLICY_WITH_SURPLUS);
        assert_eq!(d.domain, DOMAIN_BACKING);
        assert_eq!(d.bump, 254);
        assert_eq!(d.share_generation, 7);
        assert_eq!(
            decode_u40(&buf[POOL_SHARE_GENERATION_OFF..POOL_SHARE_GENERATION_OFF + 5]),
            7,
        );
        assert_eq!(d.market_slab, slab);
        assert_eq!(d.percolator_program, perc);
        assert_eq!(d.vote_authority, pool.vote_authority);
        assert_eq!(d.coin_mint, coin_mint);
        assert_eq!(d.deposit_deadline_slot, 42_424);
        assert_eq!(d.deposit_window_slots, 99);
        assert_eq!(d.deposit_start_slot, 12_345);
        assert_eq!(d.bootstrap_delay_slots, 30_000);
        assert_eq!(
            u64::from_le_bytes(
                buf[POOL_DEPOSIT_START_SLOT_OFF..POOL_DEPOSIT_START_SLOT_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            12_345,
            "Pool.deposit_start_slot must serialize at POOL_DEPOSIT_START_SLOT_OFF"
        );
        assert_eq!(
            u64::from_le_bytes(
                buf[POOL_BOOTSTRAP_DELAY_SLOTS_OFF..POOL_BOOTSTRAP_DELAY_SLOTS_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            30_000,
            "Pool.bootstrap_delay_slots must serialize at POOL_BOOTSTRAP_DELAY_SLOTS_OFF"
        );
        assert!(d.is_insurance());

        let pos = Position {
            pool: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            principal: 999,
            withdrawn_amount: 111,
            withdrawn: true,
            start_slot: 4242,
            vote_locked: true,
            terminal_returned: false,
            terminal_return_slot: None,
            share_generation: 9,
            shares: 5_555,
        };
        let mut pbuf = [0u8; POSITION_SIZE];
        pos.serialize(&mut pbuf).unwrap();
        let dp = Position::deserialize(&pbuf).unwrap();
        assert_eq!(dp.owner, pos.owner);
        assert_eq!(dp.principal, 999);
        assert!(dp.withdrawn);
        assert_eq!(dp.start_slot, 4242);
        assert!(dp.vote_locked);
        assert!(!dp.terminal_returned);
        assert_eq!(dp.share_generation, 9);
        assert_eq!(dp.shares, 5_555);
    }

    fn historical_pool_fixture() -> Pool {
        Pool {
            mint: Pubkey::new_unique(),
            asset_id: 17,
            vault: Pubkey::new_unique(),
            outstanding_principal: 91,
            policy: POLICY_WITH_SURPLUS,
            domain: DOMAIN_BACKING,
            bump: 200,
            share_generation: 17,
            market_slab: Pubkey::new_unique(),
            percolator_program: Pubkey::new_unique(),
            vote_authority: Pubkey::new_unique(),
            total_shares: 123_456,
            coin_mint: Pubkey::new_unique(),
            deposit_deadline_slot: 700,
            deposit_window_slots: 600,
            deposit_start_slot: 100,
            bootstrap_delay_slots: 1_000,
            cross_backing: false,
            custody_granted: false,
            custody_grant_slot_plus_one: 0,
            pending_backing: [0, 0],
            share_rate_numerator: 0,
            share_rate_denominator: 0,
            insurance_spent_checkpoint: 0,
            backing_protected_checkpoint: 0,
        }
    }

    #[test]
    fn terminal_claim_attestation_distinguishes_unowned_share_reserve() {
        let mut pool = historical_pool_fixture();
        pool.outstanding_principal = 0;
        pool.total_shares = 2 * VIRTUAL_SHARES;
        assert!(
            pool.owner_claims_cleared(),
            "an empty with-surplus pool's normalized reserve shares have no owner"
        );

        pool.outstanding_principal = 1;
        assert!(!pool.owner_claims_cleared());

        pool.outstanding_principal = 0;
        pool.policy = POLICY_PRINCIPAL;
        assert!(!pool.owner_claims_cleared());
        pool.total_shares = 0;
        assert!(pool.owner_claims_cleared());
    }

    #[test]
    fn cross_backing_layout_is_explicit_and_legacy_safe() {
        let mut pool = historical_pool_fixture();
        pool.policy = POLICY_PRINCIPAL;
        pool.domain = DOMAIN_INSURANCE;
        pool.cross_backing = true;
        pool.pending_backing = [3, 5];
        pool.share_rate_numerator = 1;
        pool.share_rate_denominator = VIRTUAL_SHARES;
        pool.custody_granted = true;
        pool.custody_grant_slot_plus_one = 101;
        pool.insurance_spent_checkpoint = 19;
        pool.backing_protected_checkpoint = 7;

        let mut current = [0u8; POOL_SIZE_CUSTODY_GRANT];
        pool.serialize(&mut current).unwrap();
        assert_eq!(current[POOL_FLAGS_OFF], 3);
        assert_eq!(
            u64::from_le_bytes(
                current[POOL_CUSTODY_GRANT_SLOT_OFF..POOL_CUSTODY_GRANT_SLOT_OFF + 8]
                    .try_into()
                    .unwrap()
            ),
            101,
        );
        let decoded = Pool::deserialize(&current).unwrap();
        assert!(decoded.cross_backing);
        assert!(decoded.custody_granted);
        assert_eq!(decoded.custody_grant_slot_plus_one, 101);
        assert_eq!(decoded.pending_backing, [3, 5]);
        assert_eq!(decoded.share_rate_numerator, 1);
        assert_eq!(decoded.share_rate_denominator, VIRTUAL_SHARES);
        assert_eq!(decoded.insurance_spent_checkpoint, 19);
        assert_eq!(decoded.backing_protected_checkpoint, 7);

        pool.insurance_spent_checkpoint = 0;
        pool.backing_protected_checkpoint = 0;
        let mut custody_predecessor = [0u8; POOL_SIZE_CUSTODY_GRANT_LEGACY];
        pool.serialize(&mut custody_predecessor).unwrap();
        let decoded_custody_predecessor = Pool::deserialize(&custody_predecessor).unwrap();
        assert!(decoded_custody_predecessor.cross_backing);
        assert!(decoded_custody_predecessor.custody_granted);
        assert_eq!(decoded_custody_predecessor.custody_grant_slot_plus_one, 101);
        assert_eq!(decoded_custody_predecessor.insurance_spent_checkpoint, 0);
        assert_eq!(decoded_custody_predecessor.backing_protected_checkpoint, 0);

        pool.custody_granted = false;
        pool.custody_grant_slot_plus_one = 0;
        let mut predecessor_v3 = [0u8; POOL_SIZE_CROSS_BACKING_V3];
        pool.serialize(&mut predecessor_v3).unwrap();
        let decoded_predecessor_v3 = Pool::deserialize(&predecessor_v3).unwrap();
        assert!(decoded_predecessor_v3.cross_backing);
        assert_eq!(decoded_predecessor_v3.pending_backing, [3, 5]);
        assert_eq!(decoded_predecessor_v3.share_rate_numerator, 1);
        assert_eq!(decoded_predecessor_v3.share_rate_denominator, VIRTUAL_SHARES);
        assert_eq!(decoded_predecessor_v3.insurance_spent_checkpoint, 0);
        assert_eq!(decoded_predecessor_v3.backing_protected_checkpoint, 0);

        pool.share_rate_numerator = 0;
        pool.share_rate_denominator = 0;
        let mut predecessor_v2 = [0u8; POOL_SIZE_CROSS_BACKING_V2];
        pool.serialize(&mut predecessor_v2).unwrap();
        let decoded_predecessor_v2 = Pool::deserialize(&predecessor_v2).unwrap();
        assert!(decoded_predecessor_v2.cross_backing);
        assert_eq!(decoded_predecessor_v2.pending_backing, [3, 5]);
        assert_eq!(decoded_predecessor_v2.share_rate_numerator, 0);
        assert_eq!(decoded_predecessor_v2.share_rate_denominator, 0);

        pool.pending_backing = [0, 0];
        let mut predecessor = [0u8; POOL_SIZE_CROSS_BACKING_V1];
        pool.serialize(&mut predecessor).unwrap();
        let decoded_predecessor = Pool::deserialize(&predecessor).unwrap();
        assert!(decoded_predecessor.cross_backing);
        assert_eq!(decoded_predecessor.pending_backing, [0, 0]);
        assert_eq!(decoded_predecessor.share_rate_numerator, 0);
        assert_eq!(decoded_predecessor.share_rate_denominator, 0);

        let mut legacy = [0u8; POOL_SIZE];
        assert!(
            pool.serialize(&mut legacy).is_err(),
            "a cross-backed pool cannot discard its discriminator"
        );
        pool.cross_backing = false;
        pool.serialize(&mut legacy).unwrap();
        assert!(!Pool::deserialize(&legacy).unwrap().cross_backing);

        pool.custody_granted = true;
        pool.custody_grant_slot_plus_one = 202;
        let mut standard = [0u8; POOL_SIZE_CUSTODY_GRANT];
        pool.serialize(&mut standard).unwrap();
        assert_eq!(standard[POOL_FLAGS_OFF], POOL_FLAG_CUSTODY_GRANTED);
        let decoded_standard = Pool::deserialize(&standard).unwrap();
        assert!(!decoded_standard.cross_backing);
        assert!(decoded_standard.custody_granted);
        assert_eq!(decoded_standard.custody_grant_slot_plus_one, 202);

        standard[POOL_CUSTODY_GRANT_SLOT_OFF..POOL_CUSTODY_GRANT_SLOT_OFF + 8].fill(0);
        assert!(
            Pool::deserialize(&standard).is_err(),
            "a current pool cannot claim custody without its immutable grant slot"
        );

        current[POOL_FLAGS_OFF] = 4;
        assert!(Pool::deserialize(&current).is_err());
    }

    #[test]
    fn historical_pool_and_position_layouts_round_trip_without_growth() {
        let pool = historical_pool_fixture();
        for size in [
            POOL_SIZE_BASE,
            POOL_SIZE_MARKET,
            POOL_SIZE_VOTE,
            POOL_SIZE_SHARES,
            POOL_SIZE_DEADLINE_ONLY,
            POOL_SIZE_COIN,
            POOL_SIZE_WINDOW,
            POOL_SIZE_START,
            POOL_SIZE,
            POOL_SIZE_CROSS_BACKING_V1,
            POOL_SIZE_CROSS_BACKING_V2,
            POOL_SIZE_CROSS_BACKING_V3,
            POOL_SIZE_CUSTODY_GRANT_LEGACY,
            POOL_SIZE_CROSS_BACKING,
            POOL_SIZE_CUSTODY_GRANT,
        ] {
            let mut data = vec![0u8; size];
            pool.serialize(&mut data).unwrap();
            let decoded = Pool::deserialize(&data).unwrap();
            assert_eq!(data.len(), size);
            assert_eq!(decoded.mint, pool.mint);
            assert_eq!(decoded.asset_id, pool.asset_id);
            assert_eq!(decoded.outstanding_principal, pool.outstanding_principal);
            assert_eq!(decoded.policy, pool.policy);
            assert_eq!(decoded.domain, pool.domain);
            assert_eq!(decoded.bump, pool.bump);
            assert_eq!(
                decoded.share_generation,
                if size >= POOL_SIZE_SHARES { 17 } else { 0 },
            );
            assert_eq!(
                decoded.market_slab,
                if size >= POOL_SIZE_MARKET {
                    pool.market_slab
                } else {
                    Pubkey::default()
                }
            );
            assert_eq!(
                decoded.vote_authority,
                if size >= POOL_SIZE_VOTE {
                    pool.vote_authority
                } else {
                    Pubkey::default()
                }
            );
            assert_eq!(
                decoded.total_shares,
                if size >= POOL_SIZE_SHARES {
                    pool.total_shares
                } else {
                    0
                }
            );
            assert_eq!(
                decoded.coin_mint,
                if size >= POOL_SIZE_COIN {
                    pool.coin_mint
                } else {
                    Pubkey::default()
                }
            );
            assert_eq!(
                decoded.deposit_deadline_slot,
                if size == POOL_SIZE_DEADLINE_ONLY || size >= POOL_SIZE_WINDOW {
                    pool.deposit_deadline_slot
                } else {
                    u64::MAX
                }
            );
        }

        let position = Position {
            pool: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            principal: 55,
            withdrawn_amount: 7,
            withdrawn: false,
            start_slot: 99,
            vote_locked: true,
            terminal_returned: false,
            terminal_return_slot: None,
            share_generation: 21,
            shares: 777,
        };
        for size in [POSITION_SIZE_BASE, POSITION_SIZE_TENURE, POSITION_SIZE] {
            let mut data = vec![0u8; size];
            position.serialize(&mut data).unwrap();
            let decoded = Position::deserialize(&data).unwrap();
            assert_eq!(data.len(), size);
            assert_eq!(decoded.principal, position.principal);
            assert_eq!(decoded.withdrawn_amount, position.withdrawn_amount);
            assert_eq!(
                decoded.start_slot,
                if size >= POSITION_SIZE_TENURE { 99 } else { 0 }
            );
            assert_eq!(decoded.vote_locked, size >= POSITION_SIZE_TENURE);
            assert!(!decoded.terminal_returned);
            assert_eq!(
                decoded.share_generation,
                if size >= POSITION_SIZE { 21 } else { 0 }
            );
            assert_eq!(decoded.shares, if size >= POSITION_SIZE { 777 } else { 0 });
        }

        for size in [
            95, 97, 159, 161, 191, 193, 207, 209, 215, 217, 239, 241, 255, 257, 263, 265, 271,
        ] {
            assert!(!supported_pool_size(size), "unsupported pool size {size}");
        }
        for size in [95, 97, 103, 105, 119] {
            assert!(
                !supported_position_size(size),
                "unsupported position size {size}"
            );
        }
    }

    fn canonical_pool_key(pool: &mut Pool, version: PoolSeedVersion) -> Pubkey {
        let seed_bytes = PoolSeedBytes::new(pool);
        let signer_seeds = seed_bytes.signer_seeds(pool, version);
        let (key, bump) =
            Pubkey::find_program_address(&signer_seeds[..signer_seeds.len() - 1], &crate::id());
        pool.bump = bump;
        key
    }

    #[test]
    fn same_size_historical_seed_versions_are_disambiguated_by_address() {
        let mut pool = historical_pool_fixture();
        let base_key = canonical_pool_key(&mut pool, PoolSeedVersion::Base);
        assert_eq!(
            pool_seed_version(&crate::id(), &base_key, POOL_SIZE_VOTE, &pool).unwrap(),
            PoolSeedVersion::Base
        );
        let market_key = canonical_pool_key(&mut pool, PoolSeedVersion::Market);
        assert_eq!(
            pool_seed_version(&crate::id(), &market_key, POOL_SIZE_VOTE, &pool).unwrap(),
            PoolSeedVersion::Market
        );

        let coin_key = canonical_pool_key(&mut pool, PoolSeedVersion::Coin);
        assert_eq!(
            pool_seed_version(&crate::id(), &coin_key, POOL_SIZE_COIN, &pool).unwrap(),
            PoolSeedVersion::Coin
        );
        let policy_key = canonical_pool_key(&mut pool, PoolSeedVersion::PolicyDomain);
        assert_eq!(
            pool_seed_version(&crate::id(), &policy_key, POOL_SIZE_COIN, &pool).unwrap(),
            PoolSeedVersion::PolicyDomain
        );
        assert!(
            pool_seed_version(&crate::id(), &Pubkey::new_unique(), POOL_SIZE_COIN, &pool,).is_err()
        );
    }

    #[test]
    fn cross_backing_seed_version_is_distinct_from_legacy_bootstrap() {
        let mut legacy = historical_pool_fixture();
        legacy.policy = POLICY_PRINCIPAL;
        legacy.domain = DOMAIN_INSURANCE;
        let legacy_key = canonical_pool_key(&mut legacy, PoolSeedVersion::Bootstrap);

        let mut cross = legacy;
        cross.cross_backing = true;
        let cross_key = canonical_pool_key(&mut cross, PoolSeedVersion::CrossBacking);
        assert_ne!(legacy_key, cross_key);
        assert_eq!(
            pool_seed_version(
                &crate::id(),
                &cross_key,
                POOL_SIZE_CROSS_BACKING,
                &cross,
            )
            .unwrap(),
            PoolSeedVersion::CrossBacking,
        );
        assert_eq!(
            pool_seed_version(
                &crate::id(),
                &cross_key,
                POOL_SIZE_CROSS_BACKING_V3,
                &cross,
            )
            .unwrap(),
            PoolSeedVersion::CrossBacking,
        );
        assert_eq!(
            pool_seed_version(
                &crate::id(),
                &cross_key,
                POOL_SIZE_CROSS_BACKING_V2,
                &cross,
            )
            .unwrap(),
            PoolSeedVersion::CrossBacking,
        );
        assert_eq!(
            pool_seed_version(
                &crate::id(),
                &cross_key,
                POOL_SIZE_CROSS_BACKING_V1,
                &cross,
            )
            .unwrap(),
            PoolSeedVersion::CrossBacking,
        );
        assert!(
            pool_seed_version(
                &crate::id(),
                &legacy_key,
                POOL_SIZE_CROSS_BACKING,
                &cross,
            )
            .is_err(),
            "a legacy pool address cannot authenticate cross-backed signer CPIs",
        );
    }

    // Soft-veto fairness: a depositor who joins AFTER surplus accrued cannot claim it. Property-based
    // (robust to the VIRTUAL_SHARES offset, finding HU): the offset diverts ≤1 unit/op as dust.
    #[test]
    fn shares_are_tenure_fair() {
        // Alice deposits 100 into an empty pool. 50 surplus accrues during her tenure (balance 100 ->
        // 150). Bob deposits 100 priced at 150, so gets FEWER shares (he can't buy into surplus he
        // didn't earn).
        let alice = mint_shares(100, 0, 0).unwrap();
        let bob = mint_shares(100, alice, 150).unwrap();
        assert!(
            bob < alice,
            "late bob mints fewer shares than early alice for the same principal"
        );
        let total = alice + bob;
        // Pool balance 250. Alice redeems ~her principal + the 50 tenure surplus (~150); Bob redeems
        // only ~his principal (~100), capturing ~none of the pre-existing surplus (dust to the
        // virtual offset). A late entrant cannot extract early backers' surplus — the soft-veto base.
        let alice_out = redeem_shares(alice, 250, total).unwrap();
        let bob_out = redeem_shares(bob, 250 - alice_out, total - alice).unwrap();
        assert!(
            (148..=150).contains(&alice_out),
            "alice gets principal+tenure surplus ~150: {alice_out}"
        );
        assert!(
            (99..=100).contains(&bob_out),
            "bob gets ~principal, ~0 pre-existing surplus: {bob_out}"
        );
        assert!(
            bob_out < alice_out,
            "the late depositor cannot capture the early backer's surplus"
        );
    }

    // Inflation/donation skim — the classic ERC4626 first-depositor attack — is bounded AND
    // self-defeating by the VIRTUAL_SHARES offset (findings HT/HU). An attacker seeds the empty pool
    // with 1 atom, then a large "donation" inflates the live balance, trying to make a later victim's
    // shares round toward zero so the attacker redeems the victim's principal. Two facts kill it:
    //   (1) END-TO-END the donation route doesn't even exist in genesis — the ONLY way to raise the
    //       asset-0 insurance balance without minting shares is market PnL (tenure-shared, uncontrollable
    //       by the attacker); there is no permissionless TopUp (percolator insurance is authority-gated).
    //       So the donation below is already a worst-case hypothetical the attacker cannot stage.
    //   (2) Even granting the donation, the math holds: the victim still mints non-zero shares and
    //       recovers ~all principal (skim is dust ≤ ~victim/VIRTUAL_SHARES), while the attacker loses
    //       ~half the donation to the unredeemable virtual shares — a guaranteed loss to skim dust.
    #[test]
    fn first_depositor_inflation_skim_is_bounded_and_self_defeating() {
        let attacker_deposit = 1u64;
        let donation = 1_000_000_000u64; // attacker inflates the balance (worst-case hypothetical)
        let victim_deposit = 1_000_000u64;

        let a_shares = mint_shares(attacker_deposit, 0, 0).unwrap();
        let balance_after_donation = attacker_deposit + donation; // no shares minted for the donation
        let v_shares = mint_shares(victim_deposit, a_shares, balance_after_donation).unwrap();
        assert!(
            v_shares > 0,
            "victim still mints non-zero shares — no round-to-zero griefing"
        );

        let total = a_shares + v_shares;
        let pool_balance = balance_after_donation + victim_deposit;

        // Attacker redeems first (best case for the attack), then the victim redeems the remainder.
        let a_out = redeem_shares(a_shares, pool_balance, total).unwrap();
        let v_out = redeem_shares(v_shares, pool_balance - a_out, total - a_shares).unwrap();

        // (a) The attack is strictly self-defeating: the attacker gets back less than deposit+donation.
        let a_in = attacker_deposit + donation;
        assert!(
            a_out < a_in,
            "inflation attack is self-defeating: attacker out {a_out} < in {a_in}"
        );
        // (b) The victim recovers essentially all principal — the skim is dust (« 0.1%).
        let max_skim = victim_deposit / 1000 + 2;
        assert!(
            v_out + max_skim >= victim_deposit,
            "victim recovers ~all principal: out {v_out} of {victim_deposit} (skim {})",
            victim_deposit - v_out
        );
    }

    // IMPAIRED-POOL CONSERVATION (share redemption under a market loss). POLICY_WITH_SURPLUS exits redeem
    // shares at the LIVE balance; under impairment (insurance < deposited principal) every holder takes a
    // PROPORTIONAL haircut and exits are ORDER-INDEPENDENT — no first-mover gets paid in full at the expense
    // of a stranded late exiter, and the SUM of all redemptions never exceeds the impaired balance (no
    // insolvency / over-redemption from rounding). This is the loss-direction complement of the
    // first-depositor inflation test (the gain/donation direction).
    #[test]
    fn impaired_pool_redemptions_are_pro_rata_and_conserve_no_insolvency() {
        // Three depositors fund an empty pool to a balance of 1000 principal.
        let a = mint_shares(300, 0, 0).unwrap();
        let b = mint_shares(200, a, 300).unwrap();
        let c = mint_shares(500, a + b, 500).unwrap();
        let total_shares = a + b + c;

        // A 40% market loss: the live insurance backing the pool drops 1000 -> 600.
        let mut balance: u64 = 600;
        let mut shares_left = total_shares;
        let mut redeemed: u64 = 0;
        // Exit in order a, b, c. Each nominal position retires, while only the
        // rate-safe subset leaves pool pricing shares.
        for (sh, principal) in [(a, 300u64), (b, 200), (c, 500)] {
            let owed = redeem_shares(sh, balance, shares_left).unwrap();
            // Each holder takes the SAME ~60% haircut regardless of exit order (pro-rata fairness).
            let pct = owed * 100 / principal;
            assert!(
                (59..=60).contains(&pct),
                "pro-rata haircut ~60% for principal {principal}: got {owed} ({pct}%)"
            );
            // Never pay more than the pool currently holds (no over-redemption).
            assert!(
                owed <= balance,
                "a redemption can never exceed the live balance"
            );
            let balance_after = balance - owed;
            let burned = rate_safe_pool_share_burn(sh, balance, balance_after, shares_left).unwrap();
            balance = balance_after;
            shares_left -= burned;
            redeemed += owed;
        }
        assert!(redeemed <= 600, "redemptions cannot exceed impaired insurance");
        assert!(balance <= 3, "only bounded whole-atom floor reserve remains");
        assert_eq!(redeemed + balance, 600, "tokens remain conserved");
    }
}
