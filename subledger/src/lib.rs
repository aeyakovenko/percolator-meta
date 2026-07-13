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
// an insurance pool can sign TopUpInsurance / WithdrawInsuranceLimited as the
// asset-0 insurance authority/operator. Own-vault pools leave them zero. The trailing
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
// appended, so those programs are unaffected.
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
const GENESIS_IX_ASSERT_EXECUTED: u8 = 5;
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
// Permissionless terminal return for an absent finalized-genesis depositor. It
// retires the complete position and can pay only a clean token account owned by
// that depositor.
const IX_RETURN_FINALIZED_POSITION: u8 = 12;
// Owner-signed, amountless full exit. TWAP uses this after returning established
// custody so a live owner recovery cannot commit without consuming the complete
// position that authorized it.
const IX_INSURANCE_WITHDRAW_FULL: u8 = 13;

// Percolator CPI tags (verified against the pinned v16 program, percolator-prog 624b13d).
const PERC_IX_TOP_UP_INSURANCE: u8 = 9;
// tag 57 = WithdrawInsuranceAsset { asset_index: u16, amount: u128 } — the consolidated, asset-indexed,
// insurance-operator-gated, during-Live insurance withdraw that REPLACED the removed asset-0 tag-23
// WithdrawInsuranceLimited (reconcile, finding JX/JS). The percolator caps `amount` to the available
// insurance; the subledger's own per-owner owed computation is the depositor-principal cap on top.
const PERC_IX_WITHDRAW_INSURANCE_ASSET: u8 = 57;
const PERC_IX_UPDATE_ASSET_AUTHORITY: u8 = 65;
const ASSET_AUTH_ADMIN: u8 = 0;
const ASSET_AUTH_INSURANCE: u8 = 1; // insurance_authority (gates TopUpInsurance)
const ASSET_AUTH_INSURANCE_OPERATOR: u8 = 2; // insurance_operator (gates WithdrawInsuranceLimited)
const TWAP_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TwapBuyBurn11111111111111111111111111111111");
const TWAP_IX_ACCEPT_FROM_SUBLEDGER: u8 = 15;
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
    /// Total pricing shares (POLICY_WITH_SURPLUS). A deposit mints
    /// `amount * total_shares / insurance_balance` shares (1:1 for the first); a
    /// withdraw redeems `shares * insurance_balance / total_shares`. Floor remainders
    /// leave unowned reserve shares here so an exiting holder cannot transfer that
    /// value to whoever exits last. Principal pools reset at zero principal;
    /// empty with-surplus epochs normalize reserve pricing for later deposits.
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
}

impl Pool {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if !supported_pool_size(data.len()) || data[..8] != POOL_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let policy = data[88];
        let domain = data[90];
        if policy > POLICY_WITH_SURPLUS || domain > DOMAIN_BACKING {
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
        Ok(())
    }

    fn is_insurance(&self) -> bool {
        self.percolator_program != Pubkey::default()
    }

    fn owner_claims_cleared(&self) -> bool {
        self.outstanding_principal == 0
            && match self.policy {
                POLICY_PRINCIPAL => self.total_shares == 0,
                // Empty share pools normalize any whole-atom rounding reserve
                // into unowned pricing shares for a possible later deposit epoch.
                POLICY_WITH_SURPLUS => true,
                _ => false,
            }
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

    let candidates = match pool_data_len {
        POOL_SIZE_BASE | POOL_SIZE_MARKET => BASE,
        POOL_SIZE_VOTE => BASE_OR_MARKET,
        POOL_SIZE_SHARES | POOL_SIZE_DEADLINE_ONLY => MARKET,
        POOL_SIZE_COIN => COIN_OR_POLICY,
        POOL_SIZE_WINDOW => WINDOW,
        POOL_SIZE_START => START,
        size if size >= POOL_SIZE => BOOTSTRAP,
        _ => return Err(ProgramError::InvalidAccountData),
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
    /// Live principal (current deposit, less any withdrawal). The genesis-vote
    /// reads this with `start_slot` to compute `floor(log2(now-start)) * principal`.
    principal: u64,
    withdrawn_amount: u64,
    withdrawn: bool,
    /// Last-write-time of this position (set on deposit). Topping up resets it, so
    /// late additions don't earn early-join vote weight.
    start_slot: u64,
    /// Set by the pool's vote_authority while a genesis vote is live on this
    /// position. Blocks insurance-withdraw until the vote is retracted.
    vote_locked: bool,
    /// A permissionless finalized-genesis return consumed the position. In this
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
        IX_INIT_INSURANCE_POOL => process_init_insurance_pool(program_id, accounts, &mut data),
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

    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

// withdraw accounts: [owner(s,w), pool(w), position(w), owner_ata(w), vault(w), token_program]
// data: none
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

    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
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
    position.withdrawn_amount = paid;

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
fn process_init_insurance_pool(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
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
    if policy > POLICY_WITH_SURPLUS {
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
    let (expected_pool, bump) = Pubkey::find_program_address(
        &pool_seeds_full(
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
        ),
        program_id,
    );
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
    let seeds: [&[u8]; 12] = [
        b"subledger_pool",
        mint.key.as_ref(),
        &asset_id_bytes,
        market_slab.key.as_ref(),
        percolator_program.key.as_ref(),
        coin_mint.key.as_ref(),
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
    };
    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    Ok(())
}

// insurance_deposit accounts: [owner(s,w), pool(w), position(w,pda), owner_ata(w),
//   holding(w, pool-PDA-owned token acct), market_slab(w), percolator_vault(w),
//   percolator_program, token_program, system_program]
// data: amount (u64)
//
// User -> holding (user-signed). Then the pool PDA (asset-0 insurance authority)
// signs TopUpInsurance moving holding -> Percolator insurance vault. Records the
// position (principal += amount, start_slot = now) and bumps outstanding.
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
    let percolator_program = next_account_info(iter)?;
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
    if !pool.is_insurance() {
        return Err(ProgramError::InvalidAccountData);
    }
    // Historical genesis layouts either had no deposit deadline or did not bind
    // the complete bootstrap schedule into their PDA. Upgrades preserve exits,
    // never reopen those pools to late capital.
    if pool_account.data_len() < POOL_SIZE {
        return Err(ProgramError::InvalidAccountData);
    }
    // Genesis deposits are only accepted during the configured window. Binding
    // the start slot into the pool PDA prevents a permissionless first writer
    // from opening deposits early or starting the clock before launch.
    let now = Clock::get()?.slot;
    if now < pool.deposit_start_slot || now >= pool.deposit_deadline_slot {
        return Err(ProgramError::InvalidInstructionData);
    }
    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    if *market_slab.key != pool.market_slab
        || *percolator_vault.key != pool.vault
        || *percolator_program.key != pool.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // The transit `holding` must be a `mint` token account owned by the pool PDA — the pool signs the
    // holding->vault TopUpInsurance, so a non-pool/wrong-mint holding would already make that CPI revert.
    // Validate it up front (matching insurance_withdraw) so the failure is a clear, fail-fast error rather
    // than a downstream CPI revert, and so a wrong holding can never reach the user->holding transfer.
    {
        let hs = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?;
        if hs.mint != pool.mint || hs.owner != *pool_account.key {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    // Price shares before the top-up. WITH_SURPLUS uses the complete live balance so
    // pre-existing yield stays with earlier capital. PRINCIPAL excludes protocol surplus
    // and prices only the loss-bearing portion of the pool.
    let insurance_before = read_asset0_insurance(&market_slab.try_borrow_data()?)?;
    let priced_balance_before = if pool.policy == POLICY_PRINCIPAL {
        core::cmp::min(insurance_before, pool.outstanding_principal)
    } else {
        insurance_before
    };
    begin_fully_impaired_recapitalization(&mut pool, priced_balance_before)?;
    let (shares_minted, virtual_shares) =
        mint_insurance_shares_with_capacity(&mut pool, amount, priced_balance_before)?;
    // Inflation/rounding guard (finding HB): a large surplus can make deposits mint zero or very
    // few shares. Reject before transfer unless their immediate value is within one atom of the
    // deposit, so public donations cannot turn entry rounding into material principal loss.
    require_bounded_share_rounding_with_virtual_offset(
        amount,
        shares_minted,
        pool.total_shares,
        priced_balance_before,
        virtual_shares,
    )?;

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

    // 2) holding -> Percolator insurance vault, signed by the pool PDA as the
    //    asset-0 insurance authority (TopUpInsurance, tag 9).
    let mut ix_data = vec![PERC_IX_TOP_UP_INSURANCE];
    ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
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

    pool.outstanding_principal = pool
        .outstanding_principal
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
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

    pool.serialize(&mut pool_account.try_borrow_mut_data()?)?;
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

// insurance_withdraw accounts: [owner(s,w), pool(w), position(w), owner_ata(w),
//   holding(w, pool-PDA-owned token acct), market_slab(w), percolator_vault(w),
//   vault_authority, percolator_program, token_program]
// data: amount (u64)
//
// Owner-bound, principal-only exit: `amount <= position.principal`. The pool PDA
// (asset-0 insurance operator) signs WithdrawInsuranceLimited (tag 23). NOTE: the
// real percolator handler requires the withdraw destination to be owned by the
// *operator* (the pool PDA), not an arbitrary user, so we withdraw into a
// pool-PDA-owned holding account and then SPL-transfer holding -> owner's ATA
// (pool PDA signs). Can never exceed the owner's own recorded principal.
fn process_insurance_withdraw(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    process_insurance_withdraw_impl(program_id, accounts, data, false)
}

// insurance_withdraw_full has the same accounts as insurance_withdraw and no
// instruction data. It removes the position's complete remaining principal;
// all owner, destination, pool, market, and payout checks stay centralized in
// the ordinary withdrawal implementation.
fn process_insurance_withdraw_full(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let position_account = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    if position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let principal = Position::deserialize(&position_account.try_borrow_data()?)?.principal;
    let amount_bytes = principal.to_le_bytes();
    let mut amount_data: &[u8] = &amount_bytes;
    process_insurance_withdraw_impl(program_id, accounts, &mut amount_data, false)
}

// return_finalized_position accounts: [owner, pool(w), position(w), owner_ata(w),
//   holding(w), market_slab(w), percolator_vault(w), vault_authority,
//   percolator_program, token_program, genesis_config, executed_proposal,
//   genesis_vote_program]
// data: none
//
// Once genesis is irreversibly sealed and its real market is resolved and empty,
// anyone may retire an absent depositor's complete position. The instruction has
// no amount and accepts only a clean token account owned by the depositor, so
// neither a cranker nor governance can capture or partially manipulate the payout.
fn process_return_finalized_position(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &mut &[u8],
) -> ProgramResult {
    process_insurance_withdraw_impl(program_id, accounts, data, true)
}

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
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let genesis_accounts = if terminal {
        let config = next_account_info(iter)?;
        let proposal = next_account_info(iter)?;
        let genesis_program = next_account_info(iter)?;
        if iter.next().is_some() {
            return Err(ProgramError::InvalidInstructionData);
        }
        Some((config, proposal, genesis_program))
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
    if pool_account.owner != program_id || position_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
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
    // vault_authority is a passed account, validated by PDA derivation.
    if *vault_authority.key != perc_vault_authority(market_slab.key, percolator_program.key) {
        return Err(ProgramError::InvalidSeeds);
    }
    // The holding account must be a token account for `mint` owned by the pool PDA
    // (the real percolator handler requires the withdraw dest to be the operator).
    let holding_state = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?;
    if holding_state.mint != pool.mint || holding_state.owner != *pool_account.key {
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
        let (genesis_config, executed_proposal, genesis_program) =
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
        // The genesis program owns this state and attests it read-only. A
        // successful trigger marks executed in the same transaction that seals
        // distribution, so this cannot observe a half-finalized outcome.
        invoke(
            &Instruction {
                program_id: *genesis_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*genesis_config.key, false),
                    AccountMeta::new_readonly(*executed_proposal.key, false),
                ],
                data: vec![GENESIS_IX_ASSERT_EXECUTED],
            },
            &[
                genesis_config.clone(),
                executed_proposal.clone(),
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

    // Read live asset-0 insurance from the slab. Current positions use shares priced
    // at deposit, so losses follow the capital that was present when they happened;
    // equal-entry positions remain pro-rata and exit-order independent. The full
    // requested principal leaves outstanding accounting even when the payout is impaired.
    let insurance = read_asset0_insurance(&market_slab.try_borrow_data()?)?;
    let priced_balance = if pool.policy == POLICY_PRINCIPAL {
        core::cmp::min(insurance, pool.outstanding_principal)
    } else {
        insurance
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
            payout(pool.policy, insurance, pool.outstanding_principal, amount)?
        } else {
            // Current-layout positions can legitimately have zero shares after a
            // fully impaired generation reset or a lazy scale-down. They have no
            // claim on later recapitalization and must never enter the legacy path.
            0
        }
    };
    let outstanding_after = pool
        .outstanding_principal
        .checked_sub(amount)
        .ok_or(ProgramError::InvalidAccountData)?;
    let insurance_after = insurance
        .checked_sub(owed)
        .ok_or(ProgramError::InvalidAccountData)?;
    let priced_balance_after = if pool.policy == POLICY_PRINCIPAL {
        core::cmp::min(insurance_after, outstanding_after)
    } else {
        insurance_after
    };
    let pool_shares_to_burn = if shares_to_retire == 0 {
        0
    } else {
        rate_safe_pool_share_burn_with_virtual_offset(
            shares_to_retire,
            priced_balance,
            priced_balance_after,
            pool.total_shares,
            virtual_shares,
        )?
    };

    // The pool PDA (asset-0 insurance operator) signs WithdrawInsuranceLimited,
    // moving Percolator insurance -> pool-PDA-owned holding.
    // A fully-impaired exit (owed == 0, insurance wiped) still retires the position below; only
    // move tokens when there is something to pay (percolator rejects a zero-amount withdraw).
    if owed > 0 {
        let mut ix_data = vec![PERC_IX_WITHDRAW_INSURANCE_ASSET];
        // TopUpInsurance has always credited asset 0. Historical public init accepted
        // nonzero metadata IDs, but those remain PDA seed material only; routing an
        // exit by that stale value would strand the asset-0 principal it actually funded.
        ix_data.extend_from_slice(&0u16.to_le_bytes());
        ix_data.extend_from_slice(&(owed as u128).to_le_bytes());
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

        // holding -> owner's ATA, signed by the pool PDA. The only path out, bounded by the
        // owner's pro-rata share, so the program can never pay more than is owed.
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

    // The full requested principal leaves the outstanding accounting (the loss, if any, is
    // realized); the owner collected `owed` (their pro-rata share).
    pool.outstanding_principal = outstanding_after;
    position.principal -= amount;
    // The position retires its nominal shares. The pool burns only the rate-safe
    // subset; the difference is unowned reserve that keeps floor dust out of a
    // later holder's redemption. Empty principal pools reset for terminal custody;
    // with-surplus pools normalize reserve pricing for future deposit epochs.
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
    }
    // Historical telemetry must not become a custody gate. A position can cycle
    // the finite token supply enough times for cumulative withdrawals to exceed
    // u64 even though every individual balance and principal remains valid. A
    // permissionless terminal return instead records the remaining principal at
    // risk, preserving a frozen reward cap without restoring earlier withdrawals.
    position.withdrawn_amount = if terminal {
        amount
    } else {
        position.withdrawn_amount.saturating_add(owed)
    };
    if position.principal == 0 {
        position.withdrawn = true;
        if terminal {
            // The executed proposal proves the ballot has no remaining
            // governance effect; do not leave a stale lock on retired capital.
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
    position.serialize(&mut position_account.try_borrow_mut_data()?)?;
    Ok(())
}

// accept_operator accounts: [asset_admin(signer), pool, market_slab(w), percolator_program]
// data: none
//
// The incoming pool PDA co-signs every rotation. Asset-admin moves LAST, after the
// insurance roles, so all three changes are atomic and the old externally controlled
// admin cannot recapture them after this instruction commits. This program exposes no
// arbitrary authority setter: the pool can sign only for this self-grant and the fixed
// TWAP handoff below.
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

    if !asset_admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    if !pool.is_insurance() {
        return Err(ProgramError::InvalidAccountData);
    }
    if *market_slab.key != pool.market_slab || *percolator_program.key != pool.percolator_program {
        return Err(ProgramError::InvalidAccountData);
    }
    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;
    // Receive the two insurance roles and then asset_admin. The final rotation removes
    // governance's direct path to reassign either role and withdraw principal.
    for kind in [
        ASSET_AUTH_INSURANCE,
        ASSET_AUTH_INSURANCE_OPERATOR,
        ASSET_AUTH_ADMIN,
    ] {
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

// handoff_to_twap accounts:
// [squads_vault(signer on first handoff), pool(current asset_admin), twap_config,
//  twap_authority, market_slab(w), percolator_program, twap_program]
// data: none
//
// Governance authorizes the first handoff but never receives a Percolator role. After
// an owner-bound recovery, the same current-layout TWAP config may accept the same pool
// again without governance; TWAP verifies that immutable binding before changing a
// role. The pool signs a CPI to the fixed TWAP program, which hardcodes the only
// incoming authority to its config-bound PDA and atomically protects this pool's live
// outstanding principal. POLICY_WITH_SURPLUS may cross this boundary only after every
// owner claim is gone: while principal exists the live balance is depositor share value,
// but after the final exit any later fee or rounding reserve is protocol insurance that
// otherwise has no signer-backed terminal path.
// Percolator verifies that this pool is the current asset_admin, while the TWAP verifies
// the Squads identity and all market bindings.
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
    let twap_program = next_account_info(iter)?;

    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *twap_program.key != TWAP_PROGRAM_ID || !twap_program.executable {
        return Err(ProgramError::IncorrectProgramId);
    }
    if pool_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pool = Pool::deserialize(&pool_account.try_borrow_data()?)?;
    if !pool.is_insurance()
        || (pool.policy == POLICY_WITH_SURPLUS && !pool.owner_claims_cleared())
        || *market_slab.key != pool.market_slab
        || *percolator_program.key != pool.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let pool_seed_version = validate_pool_pda(program_id, pool_account, &pool)?;

    let mut handoff_data = vec![TWAP_IX_ACCEPT_FROM_SUBLEDGER];
    handoff_data.extend_from_slice(&(pool.outstanding_principal as u128).to_le_bytes());
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

// assert_no_principal accounts: [pool]
// data: none
//
// This read-only CPI keeps the terminal owner-claim check in the program that
// owns the subledger layout. It moves no value and grants no authority.
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
