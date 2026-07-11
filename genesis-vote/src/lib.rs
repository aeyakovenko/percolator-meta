//! Non-custodial genesis vote.
//!
//! Insurance depositors vote on a COIN distribution. The deposit itself happens in
//! the `subledger` program: capital is forwarded into the Percolator market-0
//! insurance vault and a subledger *position* records attribution — owner,
//! principal, deposit slot. The funds live in Percolator, not in either program.
//! This program reads the subledger position (principal + start_slot) and the
//! subledger pool (outstanding_principal) at vote time; it never custodies funds.
//!
//! Vote: one voter, one proposal. Weight = `floor(log2(hold)) * principal`,
//! resolved at vote time (last-write-time start slot). Backing a different
//! proposal requires retracting first. Quorum = `total_voted_principal*2 >
//! outstanding`; winner = `support_weight*2 > total_cast_weight`. Exits (in the
//! subledger) shrink `outstanding`, so quorum recomputes — "those who stay decide".
//!
//! Trigger (permissionless): the first proposal to clear quorum + a weighted
//! majority is sealed by CPI into the distribution program (this program's config
//! PDA is that program's seal `authority`). No mint here — the fixed COIN supply
//! is distributed by the distribution program's claim/burn.

#![no_std]
extern crate alloc;

#[allow(unused_imports)]
use alloc::format;
use alloc::vec;

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    declare_id,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    system_instruction,
    sysvar::Sysvar,
};

declare_id!("GenesisVote11111111111111111111111111111111");

const CONFIG_DISC: [u8; 8] = *b"GVCONFG1";
const BALLOT_DISC: [u8; 8] = *b"GVBALOT1";
const PROPOSAL_DISC: [u8; 8] = *b"GVPROPV1";
const CONFIG_SIZE: usize = 264; // total_cast_weight widened u64->u128 (GG fix)
const BALLOT_SIZE: usize = 120; // voted_weight widened u64->u128 (GG fix)
const PROPOSAL_SIZE: usize = 112; // support_weight widened u64->u128 (GG fix)
const LEGACY_CONFIG_SIZE_U64: usize = 232;
const LEGACY_CONFIG_SIZE_U128: usize = 240;
const LEGACY_CONFIG_SIZE_BOOTSTRAP_END: usize = 248;
const LEGACY_BALLOT_SIZE_U64: usize = 112;
const DEFAULT_BOOTSTRAP_DELAY_SLOTS: u64 = 38_880_000;
const DEFAULT_BOOTSTRAP_START_SLOT: u64 = 0;

// Subledger position/pool discriminators + layout (read-only mirror of the
// subledger program's serialization). Used to read principal/start_slot and the
// pool's outstanding_principal at vote time.
const SUB_POSITION_DISC: [u8; 8] = *b"SUBPOS01";
const SUB_POOL_DISC: [u8; 8] = *b"SUBPOOL1";
// Mirror of the subledger Position/Pool byte offsets gv reads at vote time. UNPINNED literals here are
// the HF bug class (a stale offset reads the wrong field -> miscomputed vote weight = governance
// capture/LOF); these are cross-pinned against the subledger's canonical POS_*/POOL_* consts by
// tests/offsets.rs (finding ID — the gv counterpart of residual's offsets.rs / HL).
pub const SUB_POS_POOL_OFF: usize = 8;
pub const SUB_POS_OWNER_OFF: usize = 40;
pub const SUB_POS_PRINCIPAL_OFF: usize = 72;
pub const SUB_POS_START_SLOT_OFF: usize = 89;
pub const SUB_POOL_OUTSTANDING_OFF: usize = 80;
// Distribution proposal: disc[8], config[8..40]. Used to bind a registered vote to
// the genesis's OWN distribution config (so a winning vote is always sealable).
const DIST_PROPOSAL_DISC: [u8; 8] = *b"DISTPRP1";
// Distribution config: disc[8], coin_mint[8..40], vault[40..72], authority[72..104].
const DIST_CONFIG_DISC: [u8; 8] = *b"DISTCFG1";
// Canonical distribution program (finding IC; the gv dual of residual's HK). init_config must pin the
// wired distribution_program to THIS id — without it an attacker could deploy a fake "distribution
// program", craft a config owned by it that passes every byte check (disc/coin/authority), and
// front-run the predictable gv_config PDA to squat it, bricking the real seal (finalize DOS). The
// distribution crate is only a dev-dependency, so the id is hardcoded (matches distribution::id()).
pub const DISTRIBUTION_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("D1str1but1on11111111111111111111111111111111");

// Distribution program: SealWinner.
const DIST_IX_SEAL_WINNER: u8 = 3;

// Subledger program: SetVoteLock — pledge/release a voter's principal so a ballot
// cannot outlive the capital backing it (the config PDA is the pool vote_authority).
const SUB_IX_SET_VOTE_LOCK: u8 = 6;

const IX_INIT_CONFIG: u8 = 0;
const IX_REGISTER_PROPOSAL: u8 = 2;
const IX_VOTE: u8 = 3;
const IX_TRIGGER: u8 = 4;
// Read-only finalization attestation used by terminal subledger cleanup. This
// program owns the proposal layout, so consumers do not duplicate byte offsets.
const IX_ASSERT_EXECUTED: u8 = 5;

const VOTE_BACK: u8 = 1;
const VOTE_RETRACT: u8 = 2;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

// The gv config PDA commits to its subledger_pool, not just the COIN. init_config is
// permissionless and the config PDA = f(COIN_mint) was predictable; the distribution
// config it binds is a unique PDA f(COIN_mint) that can't be forged (distribution init
// requires the funded fixed-supply COIN), but the subledger_pool is NOT unique — an
// attacker could pass their OWN valid pool (vote_authority set to the predictable gv
// PDA, bound to a market they control post-finding-Q) and squat the gv config, pointing
// the genesis at their pool -> depositor principal misrouted (LOF) or quorum read from
// the wrong pool (DOS). Folding subledger_pool into the seed means the only gv config
// that can exist at the legit address is bound to the real pool; an attacker's pool
// lands at a different gv PDA the genesis ignores. (finding R; same class as P/Q.)
fn config_seeds<'a>(
    coin_mint: &'a Pubkey,
    subledger_pool: &'a Pubkey,
    bootstrap_delay_slots: &'a [u8; 8],
    bootstrap_start_slot: &'a [u8; 8],
) -> [&'a [u8]; 5] {
    [b"gv_config", coin_mint.as_ref(), subledger_pool.as_ref(), bootstrap_delay_slots, bootstrap_start_slot]
}
fn ballot_seeds<'a>(config: &'a Pubkey, owner: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"gv_ballot", config.as_ref(), owner.as_ref()]
}
fn sub_position_seeds<'a>(pool: &'a Pubkey, owner: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"subledger_position", pool.as_ref(), owner.as_ref()]
}
fn proposal_seeds<'a>(config: &'a Pubkey, dist_proposal: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"gv_proposal", config.as_ref(), dist_proposal.as_ref()]
}

/// Time-weighted vote power: `floor(log2(age)) * principal`. Age < 2 (or empty)
/// has no weight, so there is monotonic pressure to deposit earlier.
fn vote_weight(principal: u64, age: u64) -> u128 {
    if principal == 0 || age < 2 {
        return 0;
    }
    // u128: age.ilog2() <= 63 and principal <= u64::MAX, so the product < u128::MAX — never saturates.
    // (GG fix: a u64 weight saturated to u64::MAX, then the u64 tally overflowed on the next vote -> DOS.)
    (age.ilog2() as u128) * (principal as u128)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Config {
    coin_mint: Pubkey,
    distribution_program: Pubkey,
    distribution_config: Pubkey,
    /// The subledger program that custodies the insurance positions/pool this vote
    /// reads. Stored at init; `vote` validates the supplied accounts against it.
    subledger_program: Pubkey,
    /// The subledger insurance pool (PDA) whose positions back this vote.
    subledger_pool: Pubkey,
    /// Reserved (kept for layout stability with the seal test's init accounts).
    _reserved: Pubkey,
    total_voted_principal: u64,
    total_cast_weight: u128, // GG fix: widened so summed log-weights cannot overflow
    outstanding_principal: u64,
    bump: u8,
    bootstrap_end_slot: u64,
    bootstrap_delay_slots: u64,
    bootstrap_start_slot: u64,
}

impl Config {
    fn deserialize(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() < CONFIG_SIZE || d[..8] != CONFIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(d[8..40].try_into().unwrap()),
            distribution_program: Pubkey::new_from_array(d[40..72].try_into().unwrap()),
            distribution_config: Pubkey::new_from_array(d[72..104].try_into().unwrap()),
            subledger_program: Pubkey::new_from_array(d[104..136].try_into().unwrap()),
            subledger_pool: Pubkey::new_from_array(d[136..168].try_into().unwrap()),
            _reserved: Pubkey::new_from_array(d[168..200].try_into().unwrap()),
            total_voted_principal: u64::from_le_bytes(d[200..208].try_into().unwrap()),
            total_cast_weight: u128::from_le_bytes(d[208..224].try_into().unwrap()),
            outstanding_principal: u64::from_le_bytes(d[224..232].try_into().unwrap()),
            bump: d[232],
            bootstrap_end_slot: u64::from_le_bytes(d[233..241].try_into().unwrap()),
            bootstrap_delay_slots: u64::from_le_bytes(d[241..249].try_into().unwrap()),
            bootstrap_start_slot: u64::from_le_bytes(d[249..257].try_into().unwrap()),
        })
    }
    fn serialize(&self, d: &mut [u8]) {
        d[..8].copy_from_slice(&CONFIG_DISC);
        d[8..40].copy_from_slice(self.coin_mint.as_ref());
        d[40..72].copy_from_slice(self.distribution_program.as_ref());
        d[72..104].copy_from_slice(self.distribution_config.as_ref());
        d[104..136].copy_from_slice(self.subledger_program.as_ref());
        d[136..168].copy_from_slice(self.subledger_pool.as_ref());
        d[168..200].copy_from_slice(self._reserved.as_ref());
        d[200..208].copy_from_slice(&self.total_voted_principal.to_le_bytes());
        d[208..224].copy_from_slice(&self.total_cast_weight.to_le_bytes());
        d[224..232].copy_from_slice(&self.outstanding_principal.to_le_bytes());
        d[232] = self.bump;
        d[233..241].copy_from_slice(&self.bootstrap_end_slot.to_le_bytes());
        d[241..249].copy_from_slice(&self.bootstrap_delay_slots.to_le_bytes());
        d[249..257].copy_from_slice(&self.bootstrap_start_slot.to_le_bytes());
        d[257..CONFIG_SIZE].fill(0);
    }
}

fn read_bootstrap_schedule(data: &[u8]) -> Result<(u64, u64), ProgramError> {
    match data.len() {
        0 => Ok((DEFAULT_BOOTSTRAP_DELAY_SLOTS, DEFAULT_BOOTSTRAP_START_SLOT)),
        16 => {
            let delay = u64::from_le_bytes(data[..8].try_into().unwrap());
            let start = u64::from_le_bytes(data[8..16].try_into().unwrap());
            if delay == 0 {
                return Err(ProgramError::InvalidInstructionData);
            }
            start.checked_add(delay).ok_or(ProgramError::ArithmeticOverflow)?;
            Ok((delay, start))
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// Per-voter ballot, owned by this program. Holds only the vote state — the
/// principal/start_slot live in the subledger position (read at vote time), so
/// this program never duplicates the deposit ledger.
struct Ballot {
    owner: Pubkey,
    voted_proposal: Pubkey, // default() = no live ballot
    voted_weight: u128, // GG fix: widened to match the u128 weight tallies for an exact retract back-out
    voted_principal: u64,
}

impl Ballot {
    fn deserialize(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() < BALLOT_SIZE || d[..8] != BALLOT_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            owner: Pubkey::new_from_array(d[8..40].try_into().unwrap()),
            voted_proposal: Pubkey::new_from_array(d[40..72].try_into().unwrap()),
            voted_weight: u128::from_le_bytes(d[72..88].try_into().unwrap()),
            voted_principal: u64::from_le_bytes(d[88..96].try_into().unwrap()),
        })
    }
    fn serialize(&self, d: &mut [u8]) {
        d[..8].copy_from_slice(&BALLOT_DISC);
        d[8..40].copy_from_slice(self.owner.as_ref());
        d[40..72].copy_from_slice(self.voted_proposal.as_ref());
        d[72..88].copy_from_slice(&self.voted_weight.to_le_bytes());
        d[88..96].copy_from_slice(&self.voted_principal.to_le_bytes());
        d[96..BALLOT_SIZE].fill(0);
    }
    fn has_live_ballot(&self) -> bool {
        self.voted_proposal != Pubkey::default()
    }
}

/// Read `(principal, start_slot)` from a subledger position account. The subledger
/// position layout is: disc[8], pool[32], owner[32], principal(u64), withdrawn(u64),
/// withdrawn_flag(u8), start_slot(u64@89).
fn read_sub_position(
    data: &[u8],
    expected_pool: &Pubkey,
    expected_owner: &Pubkey,
) -> Result<(u64, u64), ProgramError> {
    if data.len() < 97 || data[..8] != SUB_POSITION_DISC {
        return Err(ProgramError::InvalidAccountData);
    }
    let pool = Pubkey::new_from_array(data[SUB_POS_POOL_OFF..SUB_POS_POOL_OFF + 32].try_into().unwrap());
    let owner = Pubkey::new_from_array(data[SUB_POS_OWNER_OFF..SUB_POS_OWNER_OFF + 32].try_into().unwrap());
    if pool != *expected_pool || owner != *expected_owner {
        return Err(ProgramError::InvalidAccountData);
    }
    let principal = u64::from_le_bytes(data[SUB_POS_PRINCIPAL_OFF..SUB_POS_PRINCIPAL_OFF + 8].try_into().unwrap());
    let start_slot = u64::from_le_bytes(data[SUB_POS_START_SLOT_OFF..SUB_POS_START_SLOT_OFF + 8].try_into().unwrap());
    Ok((principal, start_slot))
}

/// Read `outstanding_principal` from a subledger pool account. The subledger pool
/// layout is: disc[8], mint[32], asset_id(u64), vault[32], outstanding(u64@80).
fn read_sub_pool_outstanding(data: &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 88 || data[..8] != SUB_POOL_DISC {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(u64::from_le_bytes(data[SUB_POOL_OUTSTANDING_OFF..SUB_POOL_OUTSTANDING_OFF + 8].try_into().unwrap()))
}

struct ProposalVote {
    config: Pubkey,
    distribution_proposal: Pubkey,
    support_weight: u128, // GG fix: widened so summed log-weights cannot overflow
    support_principal: u64,
    executed: bool,
    /// Snapshot of the distribution proposal's (entry_count, total_amount) at
    /// registration. The trigger requires these to be UNCHANGED at seal time, so a
    /// creator cannot append self-allocations AFTER voters have backed the proposal
    /// (a bait-and-switch on the distribution voters approved).
    snapshot_entry_count: u32,
    snapshot_total_amount: u64,
}

impl ProposalVote {
    fn deserialize(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() < PROPOSAL_SIZE || d[..8] != PROPOSAL_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let executed = d[96];
        if executed > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            config: Pubkey::new_from_array(d[8..40].try_into().unwrap()),
            distribution_proposal: Pubkey::new_from_array(d[40..72].try_into().unwrap()),
            support_weight: u128::from_le_bytes(d[72..88].try_into().unwrap()),
            support_principal: u64::from_le_bytes(d[88..96].try_into().unwrap()),
            executed: executed == 1,
            snapshot_entry_count: u32::from_le_bytes(d[97..101].try_into().unwrap()),
            snapshot_total_amount: u64::from_le_bytes(d[101..109].try_into().unwrap()),
        })
    }
    fn serialize(&self, d: &mut [u8]) {
        d[..8].copy_from_slice(&PROPOSAL_DISC);
        d[8..40].copy_from_slice(self.config.as_ref());
        d[40..72].copy_from_slice(self.distribution_proposal.as_ref());
        d[72..88].copy_from_slice(&self.support_weight.to_le_bytes());
        d[88..96].copy_from_slice(&self.support_principal.to_le_bytes());
        d[96] = self.executed as u8;
        d[97..101].copy_from_slice(&self.snapshot_entry_count.to_le_bytes());
        d[101..109].copy_from_slice(&self.snapshot_total_amount.to_le_bytes());
        d[109..PROPOSAL_SIZE].fill(0);
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn process_instruction<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    instruction_data: &[u8],
) -> ProgramResult {
    let (tag, data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        IX_INIT_CONFIG => init_config(program_id, accounts, data),
        IX_REGISTER_PROPOSAL => register_proposal(program_id, accounts),
        IX_VOTE => vote(program_id, accounts, data),
        IX_TRIGGER => trigger(program_id, accounts, data),
        IX_ASSERT_EXECUTED => assert_executed(program_id, accounts, data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// assert_executed accounts: [config, proposal_vote]
// data: none
//
// A read-only proof that this exact current-layout genesis config has sealed the
// supplied proposal. It grants no signer privilege and mutates no state.
fn assert_executed(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let config_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if config_account.owner != program_id || proposal_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let proposal = ProposalVote::deserialize(&proposal_account.try_borrow_data()?)?;

    let delay = config.bootstrap_delay_slots.to_le_bytes();
    let start = config.bootstrap_start_slot.to_le_bytes();
    let (expected_config, bump) = Pubkey::find_program_address(
        &config_seeds(&config.coin_mint, &config.subledger_pool, &delay, &start),
        program_id,
    );
    if expected_config != *config_account.key || bump != config.bump {
        return Err(ProgramError::InvalidSeeds);
    }
    let expected_proposal = Pubkey::find_program_address(
        &proposal_seeds(config_account.key, &proposal.distribution_proposal),
        program_id,
    )
    .0;
    if expected_proposal != *proposal_account.key
        || proposal.config != *config_account.key
        || !proposal.executed
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

// Create a program-owned PDA, tolerating an attacker pre-funding the (deterministic) address.
// System `create_account` aborts with AccountAlreadyInUse on ANY pre-existing lamports, so a 1-
// lamport transfer to the address (no signature needed) would PERMANENTLY brick init — the lamports
// can never be swept from a system-owned PDA. Top up the rent shortfall (a plain transfer) then
// allocate + assign via invoke_signed; both only require the account to be data-empty + system-owned,
// true for a merely pre-funded address. Callers gate re-init on `data_len() != 0` (NOT lamports).
// (finding AI)
fn create_pda<'a>(
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
    )
}

// init_config accounts: [payer(s,w), coin_mint, config(pda,w), distribution_program,
//   distribution_config, subledger_program, subledger_pool, reserved, system]
// data: absent = default schedule; otherwise bootstrap_delay_slots(u64),
// bootstrap_start_slot(u64). The schedule is part of the config PDA so a
// permissionless first writer cannot shorten or early-start the bootstrap clock.
fn init_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let distribution_program = next_account_info(iter)?;
    let distribution_config = next_account_info(iter)?;
    let subledger_program = next_account_info(iter)?;
    let subledger_pool = next_account_info(iter)?;
    let reserved = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let (bootstrap_delay_slots, bootstrap_start_slot) = read_bootstrap_schedule(data)?;
    let bootstrap_end_slot = bootstrap_start_slot
        .checked_add(bootstrap_delay_slots)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let delay_bytes = bootstrap_delay_slots.to_le_bytes();
    let start_bytes = bootstrap_start_slot.to_le_bytes();
    let (expected, bump) = Pubkey::find_program_address(
        &config_seeds(coin_mint.key, subledger_pool.key, &delay_bytes, &start_bytes),
        program_id,
    );
    if *config_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if config_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    if bootstrap_end_slot <= Clock::get()?.slot {
        return Err(ProgramError::InvalidInstructionData);
    }

    // Bind the wired dependencies back to THIS config so a genesis can never be
    // built on poisoned or foreign accounts. Without these, an honest orchestrator
    // could unknowingly point the config at:
    //  - a subledger pool whose vote_authority is NOT this config PDA -> every vote's
    //    SetVoteLock CPI fails -> voting bricks (and the pool may be attacker-set,
    //    cf. finding G); or
    //  - a distribution config whose seal authority is NOT this config PDA, or for a
    //    different mint -> trigger's SealWinner can never succeed -> finalize DOS.
    // The config PDA (`expected`) must be the distribution seal authority AND the
    // subledger pool's vote_authority, both for this coin_mint.
    // Pin the distribution program to the canonical id (finding IC) BEFORE trusting `owner ==
    // distribution_program`: otherwise a fake program could own a byte-perfect squat config.
    if *distribution_program.key != DISTRIBUTION_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    {
        let dc = distribution_config.try_borrow_data()?;
        if distribution_config.owner != distribution_program.key
            || dc.len() < 104
            || dc[..8] != DIST_CONFIG_DISC
            || Pubkey::new_from_array(dc[8..40].try_into().unwrap()) != *coin_mint.key
            || Pubkey::new_from_array(dc[72..104].try_into().unwrap()) != expected
        {
            return Err(ProgramError::InvalidAccountData);
        }
    }
    {
        // NOTE: the subledger pool holds the at-risk COLLATERAL, which is a DIFFERENT
        // mint from the distributed COIN (README money map). So we do NOT bind the
        // pool's mint to coin_mint — the security-critical binding is that the pool's
        // vote_authority is THIS config PDA (so the genesis can't be wired to a
        // poisoned/foreign pool, findings G/H), not which token it holds.
        let sp = subledger_pool.try_borrow_data()?;
        if subledger_pool.owner != subledger_program.key
            || sp.len() < 192
            || sp[..8] != SUB_POOL_DISC
            || Pubkey::new_from_array(sp[160..192].try_into().unwrap()) != expected
        {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    let bump_arr = [bump];
    let seeds: [&[u8]; 6] = [b"gv_config", coin_mint.key.as_ref(), subledger_pool.key.as_ref(), &delay_bytes, &start_bytes, &bump_arr];
    create_pda(payer, config_account, system_program, program_id, &seeds, CONFIG_SIZE)?;

    let config = Config {
        coin_mint: *coin_mint.key,
        distribution_program: *distribution_program.key,
        distribution_config: *distribution_config.key,
        subledger_program: *subledger_program.key,
        subledger_pool: *subledger_pool.key,
        _reserved: *reserved.key,
        total_voted_principal: 0,
        total_cast_weight: 0,
        outstanding_principal: 0,
        bump,
        bootstrap_end_slot,
        bootstrap_delay_slots,
        bootstrap_start_slot,
    };
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// register_proposal accounts: [payer(s,w), config, proposal_vote(pda,w),
//   distribution_proposal, system]
fn register_proposal<'a>(program_id: &Pubkey, accounts: &'a [AccountInfo<'a>]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;
    let distribution_proposal = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    // The proposal must be a real distribution proposal owned by the distribution
    // program (so votes can only target genuine distributions).
    if distribution_proposal.owner != &config.distribution_program {
        return Err(ProgramError::IllegalOwner);
    }
    // ...AND it must belong to THIS genesis's distribution config. Otherwise a vote
    // could be registered against a foreign distribution proposal (owned by the same
    // program but under a different config); if it then won, `trigger` would CPI
    // SealWinner(config.distribution_config, foreign_proposal), which the distribution
    // rejects (header.config mismatch) — bricking finalize forever. Bind it here so
    // every votable proposal is guaranteed sealable.
    // Snapshot the proposal's (entry_count, total_amount) so the trigger can verify
    // it is UNCHANGED at seal time — a creator must not append self-allocations after
    // voters back it (bait-and-switch). Require it non-empty: only a fully-built
    // proposal can be registered for voting.
    let (snapshot_entry_count, snapshot_total_amount) = {
        let pd = distribution_proposal.try_borrow_data()?;
        if pd.len() < 96 || pd[..8] != DIST_PROPOSAL_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let dist_proposal_config = Pubkey::new_from_array(pd[8..40].try_into().unwrap());
        if dist_proposal_config != config.distribution_config {
            return Err(ProgramError::InvalidAccountData);
        }
        // Only the proposal's CREATOR may register it for voting (creator at [48..80]
        // of the distribution proposal header). register is otherwise permissionless,
        // so an attacker could register a creator's PARTIALLY-built proposal, freezing
        // the snapshot at a stale (entry_count, total_amount); the creator's next
        // append would then make the live proposal mismatch the snapshot and `trigger`
        // would reject it forever (front-run griefing DOS). Binding registration to the
        // creator means they register only once the proposal is complete.
        let creator = Pubkey::new_from_array(pd[48..80].try_into().unwrap());
        if creator != *payer.key {
            return Err(ProgramError::IllegalOwner);
        }
        let entry_count = u32::from_le_bytes(pd[84..88].try_into().unwrap());
        let total_amount = u64::from_le_bytes(pd[88..96].try_into().unwrap());
        if entry_count == 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        (entry_count, total_amount)
    };

    let seeds = proposal_seeds(config_account.key, distribution_proposal.key);
    let (expected, bump) = Pubkey::find_program_address(&seeds, program_id);
    if *proposal_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if proposal_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let bump_arr = [bump];
    let s: [&[u8]; 4] = [b"gv_proposal", config_account.key.as_ref(), distribution_proposal.key.as_ref(), &bump_arr];
    create_pda(payer, proposal_account, system_program, program_id, &s, PROPOSAL_SIZE)?;

    let pv = ProposalVote {
        config: *config_account.key,
        distribution_proposal: *distribution_proposal.key,
        support_weight: 0,
        support_principal: 0,
        executed: false,
        snapshot_entry_count,
        snapshot_total_amount,
    };
    pv.serialize(&mut proposal_account.try_borrow_mut_data()?);
    Ok(())
}

// Historical configs remain deliberately inert: they cannot accept new backing or
// trigger a distribution under the current binary. Retraction is the sole exception
// because the config PDA is also a subledger vote-lock authority; rejecting its old
// layout would permanently freeze the owner's principal after an upgrade.
fn retract_legacy_vote<'a>(
    program_id: &Pubkey,
    voter: &AccountInfo<'a>,
    config_account: &AccountInfo<'a>,
    ballot_account: &AccountInfo<'a>,
    proposal_account: &AccountInfo<'a>,
    sub_position: &AccountInfo<'a>,
    sub_pool: &AccountInfo<'a>,
    subledger_program: &AccountInfo<'a>,
) -> ProgramResult {
    let (coin_mint, configured_subledger, configured_pool, bump, coin_only, ballot_size, contribution_end) = {
        let d = config_account.try_borrow_data()?;
        let (bump_offset, ballot_size, contribution_end) = match d.len() {
            LEGACY_CONFIG_SIZE_U64 => (224, LEGACY_BALLOT_SIZE_U64, 88),
            LEGACY_CONFIG_SIZE_U128 | LEGACY_CONFIG_SIZE_BOOTSTRAP_END => {
                (232, BALLOT_SIZE, 96)
            }
            _ => return Err(ProgramError::InvalidAccountData),
        };
        if d[..8] != CONFIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let coin_mint = Pubkey::new_from_array(d[8..40].try_into().unwrap());
        let configured_subledger = Pubkey::new_from_array(d[104..136].try_into().unwrap());
        let configured_pool = Pubkey::new_from_array(d[136..168].try_into().unwrap());
        let bump = d[bump_offset];
        let (pool_key, pool_bump) = Pubkey::find_program_address(
            &[b"gv_config", coin_mint.as_ref(), configured_pool.as_ref()],
            program_id,
        );
        let pool_match = *config_account.key == pool_key && bump == pool_bump;

        // The 232-byte generation spans the one historical PDA transition: early
        // configs were keyed by COIN only, then a75b0ab bound the subledger pool.
        let coin_only = if d.len() == LEGACY_CONFIG_SIZE_U64 {
            let (coin_key, coin_bump) =
                Pubkey::find_program_address(&[b"gv_config", coin_mint.as_ref()], program_id);
            match (*config_account.key == coin_key && bump == coin_bump, pool_match) {
                (true, false) => true,
                (false, true) => false,
                _ => return Err(ProgramError::InvalidSeeds),
            }
        } else if pool_match {
            false
        } else {
            return Err(ProgramError::InvalidSeeds);
        };
        (coin_mint, configured_subledger, configured_pool, bump, coin_only, ballot_size, contribution_end)
    };

    if ballot_account.owner != program_id
        || sub_position.owner != &configured_subledger
        || sub_pool.owner != &configured_subledger
    {
        return Err(ProgramError::IllegalOwner);
    }
    if *sub_pool.key != configured_pool
        || *subledger_program.key != configured_subledger
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let (expected_sub_position, _) = Pubkey::find_program_address(
        &sub_position_seeds(sub_pool.key, voter.key),
        &configured_subledger,
    );
    if *sub_position.key != expected_sub_position {
        return Err(ProgramError::InvalidSeeds);
    }
    read_sub_position(&sub_position.try_borrow_data()?, sub_pool.key, voter.key)?;

    let (expected_ballot, _) =
        Pubkey::find_program_address(&ballot_seeds(config_account.key, voter.key), program_id);
    if *ballot_account.key != expected_ballot {
        return Err(ProgramError::InvalidSeeds);
    }
    let voted_proposal = {
        let ballot = ballot_account.try_borrow_data()?;
        if ballot.len() != ballot_size || ballot[..8] != BALLOT_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        if Pubkey::new_from_array(ballot[8..40].try_into().unwrap()) != *voter.key {
            return Err(ProgramError::IllegalOwner);
        }
        Pubkey::new_from_array(ballot[40..72].try_into().unwrap())
    };
    if voted_proposal == Pubkey::default() || voted_proposal != *proposal_account.key {
        return Err(ProgramError::InvalidInstructionData);
    }

    let unlock_ix = Instruction {
        program_id: configured_subledger,
        accounts: vec![
            AccountMeta::new_readonly(*config_account.key, true),
            AccountMeta::new_readonly(*sub_pool.key, false),
            AccountMeta::new(*sub_position.key, false),
            AccountMeta::new_readonly(*voter.key, true),
        ],
        data: vec![SUB_IX_SET_VOTE_LOCK, 0],
    };
    let cpi_accounts = [
        config_account.clone(),
        sub_pool.clone(),
        sub_position.clone(),
        voter.clone(),
        subledger_program.clone(),
    ];
    let bump_arr = [bump];
    if coin_only {
        let seeds: [&[u8]; 3] = [b"gv_config", coin_mint.as_ref(), &bump_arr];
        invoke_signed(&unlock_ix, &cpi_accounts, &[&seeds])?;
    } else {
        let seeds: [&[u8]; 4] = [
            b"gv_config",
            coin_mint.as_ref(),
            configured_pool.as_ref(),
            &bump_arr,
        ];
        invoke_signed(&unlock_ix, &cpi_accounts, &[&seeds])?;
    }

    // Do not reinterpret or rewrite old global tallies. No current instruction can
    // use those generations for voting/triggering, while clearing this owner-bound
    // ballot makes the one recovery action non-replayable.
    ballot_account.try_borrow_mut_data()?[40..contribution_end].fill(0);
    Ok(())
}

// vote accounts: [voter(s,w), config(w), ballot(w,pda), proposal_vote(w),
//   sub_position(w), sub_pool(ro), system_program, subledger_program]
// data: action(u8) — 1 back, 2 retract
//
// After updating the ballot, the config PDA CPIs the subledger to set/clear the
// position's vote-lock: a live ballot locks the principal (no insurance-withdraw
// until retracted), so a vote can never outlive the capital backing it.
//
// Reads the voter's principal + start_slot from the subledger position and the
// pool's outstanding_principal from the subledger pool (validated by program owner
// + PDA derivation). The quorum denominator is synced from the pool each vote, so
// subledger exits that shrink outstanding are reflected here.
fn vote<'a>(program_id: &Pubkey, accounts: &'a [AccountInfo<'a>], data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let voter = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let ballot_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;
    let sub_position = next_account_info(iter)?;
    let sub_pool = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let subledger_program = next_account_info(iter)?;

    if data.len() != 1 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let action = data[0];
    if action != VOTE_BACK && action != VOTE_RETRACT {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !voter.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id || proposal_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if action == VOTE_RETRACT
        && matches!(
            config_account.data_len(),
            LEGACY_CONFIG_SIZE_U64
                | LEGACY_CONFIG_SIZE_U128
                | LEGACY_CONFIG_SIZE_BOOTSTRAP_END
        )
    {
        return retract_legacy_vote(
            program_id,
            voter,
            config_account,
            ballot_account,
            proposal_account,
            sub_position,
            sub_pool,
            subledger_program,
        );
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let mut pv = ProposalVote::deserialize(&proposal_account.try_borrow_data()?)?;
    if pv.config != *config_account.key {
        return Err(ProgramError::InvalidAccountData);
    }
    // Once the winner is sealed, no NEW backing — but a voter must always be able to
    // RETRACT, even post-seal: retract clears the subledger vote-lock so they can
    // exit their principal. Blocking retract here would freeze winning voters'
    // capital forever (their proposal is the executed one). The seal is immutable;
    // post-seal tally mutations are harmless (nothing reads them after execution).
    if pv.executed && action == VOTE_BACK {
        return Err(ProgramError::InvalidAccountData);
    }

    // The subledger position + pool must be owned by the configured subledger
    // program and be the canonical PDAs for (pool, voter) / (pool).
    if sub_position.owner != &config.subledger_program || sub_pool.owner != &config.subledger_program {
        return Err(ProgramError::IllegalOwner);
    }
    if *sub_pool.key != config.subledger_pool || *subledger_program.key != config.subledger_program {
        return Err(ProgramError::InvalidAccountData);
    }
    let (expected_sub_pos, _) = Pubkey::find_program_address(
        &sub_position_seeds(sub_pool.key, voter.key),
        &config.subledger_program,
    );
    if *sub_position.key != expected_sub_pos {
        return Err(ProgramError::InvalidSeeds);
    }
    let (principal, start_slot) =
        read_sub_position(&sub_position.try_borrow_data()?, sub_pool.key, voter.key)?;
    // Snapshot the live pool outstanding into the config for off-chain visibility. NOTE: this is NOT
    // the quorum denominator — `trigger` deliberately RE-READS the live pool outstanding at seal time
    // (see read_sub_pool_outstanding there), never this stored field, so a late deposit/exit between the
    // last vote and the trigger is always reflected. Using this cached value would reintroduce the
    // stale-low minority-capture hole that `trigger_uses_live_pool_outstanding_not_stale_cache` guards.
    config.outstanding_principal = read_sub_pool_outstanding(&sub_pool.try_borrow_data()?)?;

    // Ballot PDA (one per voter per config). Created lazily on first back.
    let (expected_ballot, ballot_bump) =
        Pubkey::find_program_address(&ballot_seeds(config_account.key, voter.key), program_id);
    if *ballot_account.key != expected_ballot {
        return Err(ProgramError::InvalidSeeds);
    }
    let mut ballot = if ballot_account.data_len() == 0 {
        if action == VOTE_RETRACT {
            return Err(ProgramError::InvalidInstructionData); // nothing to retract
        }
        let bump_arr = [ballot_bump];
        let s: [&[u8]; 4] = [b"gv_ballot", config_account.key.as_ref(), voter.key.as_ref(), &bump_arr];
        create_pda(voter, ballot_account, system_program, program_id, &s, BALLOT_SIZE)?;
        Ballot {
            owner: *voter.key,
            voted_proposal: Pubkey::default(),
            voted_weight: 0,
            voted_principal: 0,
        }
    } else {
        if ballot_account.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let b = Ballot::deserialize(&ballot_account.try_borrow_data()?)?;
        if b.owner != *voter.key {
            return Err(ProgramError::IllegalOwner);
        }
        b
    };

    // One vote, one proposal: a live ballot must be on THIS proposal.
    if ballot.has_live_ballot() && ballot.voted_proposal != *proposal_account.key {
        msg!("retract your existing vote before backing another proposal");
        return Err(ProgramError::InvalidInstructionData);
    }

    // Back out any prior live contribution from this proposal + the global tallies.
    if ballot.has_live_ballot() {
        pv.support_weight = pv.support_weight.checked_sub(ballot.voted_weight).ok_or(ProgramError::InvalidAccountData)?;
        pv.support_principal = pv.support_principal.checked_sub(ballot.voted_principal).ok_or(ProgramError::InvalidAccountData)?;
        config.total_cast_weight = config.total_cast_weight.checked_sub(ballot.voted_weight).ok_or(ProgramError::InvalidAccountData)?;
        config.total_voted_principal = config.total_voted_principal.checked_sub(ballot.voted_principal).ok_or(ProgramError::InvalidAccountData)?;
    } else if action == VOTE_RETRACT {
        return Err(ProgramError::InvalidInstructionData); // nothing to retract
    }

    if action == VOTE_RETRACT {
        ballot.voted_proposal = Pubkey::default();
        ballot.voted_weight = 0;
        ballot.voted_principal = 0;
    } else {
        let clock = Clock::get()?;
        // Slot zero is a valid configured bootstrap start and deposit timestamp. Principal zero
        // and age below two already produce zero weight, so no timestamp sentinel is needed.
        let weight = vote_weight(principal, clock.slot.saturating_sub(start_slot));
        if weight == 0 {
            msg!("position has no vote weight (unfunded or too recent)");
            return Err(ProgramError::InvalidAccountData);
        }
        pv.support_weight = pv.support_weight.checked_add(weight).ok_or(ProgramError::ArithmeticOverflow)?;
        pv.support_principal = pv.support_principal.checked_add(principal).ok_or(ProgramError::ArithmeticOverflow)?;
        config.total_cast_weight = config.total_cast_weight.checked_add(weight).ok_or(ProgramError::ArithmeticOverflow)?;
        config.total_voted_principal = config.total_voted_principal.checked_add(principal).ok_or(ProgramError::ArithmeticOverflow)?;
        ballot.voted_proposal = *proposal_account.key;
        ballot.voted_weight = weight;
        ballot.voted_principal = principal;
    }

    config.serialize(&mut config_account.try_borrow_mut_data()?);
    ballot.serialize(&mut ballot_account.try_borrow_mut_data()?);
    pv.serialize(&mut proposal_account.try_borrow_mut_data()?);

    // Pledge (back) or release (retract) the voter's principal in the subledger so a
    // live ballot is always backed by capital still at risk. The config PDA is the
    // pool's vote_authority; it can only toggle the lock, never move funds.
    let lock_val: u8 = if ballot.has_live_ballot() { 1 } else { 0 };
    let bump_arr = [config.bump];
    let delay_bytes = config.bootstrap_delay_slots.to_le_bytes();
    let start_bytes = config.bootstrap_start_slot.to_le_bytes();
    let seeds: [&[u8]; 6] = [b"gv_config", config.coin_mint.as_ref(), config.subledger_pool.as_ref(), &delay_bytes, &start_bytes, &bump_arr];
    invoke_signed(
        &Instruction {
            program_id: config.subledger_program,
            accounts: vec![
                AccountMeta::new_readonly(*config_account.key, true),
                AccountMeta::new_readonly(*sub_pool.key, false),
                AccountMeta::new(*sub_position.key, false),
                // The voter signs the outer tx; propagate that signature so the
                // subledger can require owner consent for the (un)lock.
                AccountMeta::new_readonly(*voter.key, true),
            ],
            data: vec![SUB_IX_SET_VOTE_LOCK, lock_val],
        },
        &[
            config_account.clone(),
            sub_pool.clone(),
            sub_position.clone(),
            voter.clone(),
            subledger_program.clone(),
        ],
        &[&seeds],
    )?;
    Ok(())
}

// trigger accounts: [cranker(s), config(w), proposal_vote(w), distribution_program,
//   distribution_config(w), distribution_proposal(w)]
// Permissionless: seal the winning distribution via CPI.
fn trigger<'a>(program_id: &Pubkey, accounts: &'a [AccountInfo<'a>], data: &[u8]) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;
    let distribution_program = next_account_info(iter)?;
    let distribution_config = next_account_info(iter)?;
    let distribution_proposal = next_account_info(iter)?;
    let sub_pool = next_account_info(iter)?;

    if !cranker.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id || proposal_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let mut pv = ProposalVote::deserialize(&proposal_account.try_borrow_data()?)?;
    if pv.config != *config_account.key || pv.executed {
        return Err(ProgramError::InvalidAccountData);
    }
    if Clock::get()?.slot < config.bootstrap_end_slot {
        msg!("bootstrap delay has not elapsed");
        return Err(ProgramError::InvalidInstructionData);
    }
    if *distribution_program.key != config.distribution_program
        || *distribution_config.key != config.distribution_config
        || *distribution_proposal.key != pv.distribution_proposal
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // Anti bait-and-switch: the distribution proposal must be UNCHANGED since it was
    // registered. If the creator appended self-allocations after voters backed it,
    // the (entry_count, total_amount) snapshot won't match and the seal is refused —
    // so the sealed distribution is exactly the one voters approved.
    {
        let pd = distribution_proposal.try_borrow_data()?;
        if pd.len() < 96
            || u32::from_le_bytes(pd[84..88].try_into().unwrap()) != pv.snapshot_entry_count
            || u64::from_le_bytes(pd[88..96].try_into().unwrap()) != pv.snapshot_total_amount
        {
            msg!("distribution proposal changed after registration");
            return Err(ProgramError::InvalidAccountData);
        }
    }
    // Quorum is measured against the LIVE subledger pool outstanding, not the
    // cached config value. The cache is only refreshed on votes, so a stale-low
    // cache would let a minority that voted early capture the distribution after
    // honest deposits grow the pool without a re-vote. Re-read the live pool here.
    if sub_pool.owner != &config.subledger_program || *sub_pool.key != config.subledger_pool {
        return Err(ProgramError::InvalidAccountData);
    }
    let live_outstanding = read_sub_pool_outstanding(&sub_pool.try_borrow_data()?)?;
    // Quorum: more than half of the live outstanding insurance principal has voted.
    if (config.total_voted_principal as u128) * 2 <= live_outstanding as u128 {
        msg!("vote lacks a principal quorum");
        return Err(ProgramError::InvalidInstructionData);
    }
    // Winner: this proposal holds a strict majority of cast log-weight.
    if (pv.support_weight as u128) * 2 <= config.total_cast_weight as u128 {
        msg!("proposal lacks a weighted majority");
        return Err(ProgramError::InvalidInstructionData);
    }

    pv.executed = true;
    pv.serialize(&mut proposal_account.try_borrow_mut_data()?);

    // Seal the distribution. The config PDA is the distribution's seal authority.
    let bump_arr = [config.bump];
    let delay_bytes = config.bootstrap_delay_slots.to_le_bytes();
    let start_bytes = config.bootstrap_start_slot.to_le_bytes();
    let seeds: [&[u8]; 6] = [b"gv_config", config.coin_mint.as_ref(), config.subledger_pool.as_ref(), &delay_bytes, &start_bytes, &bump_arr];
    invoke_signed(
        &Instruction {
            program_id: *distribution_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*config_account.key, true),
                AccountMeta::new(*distribution_config.key, false),
                AccountMeta::new(*distribution_proposal.key, false),
            ],
            data: vec![DIST_IX_SEAL_WINNER],
        },
        &[
            config_account.clone(),
            distribution_config.clone(),
            distribution_proposal.clone(),
            distribution_program.clone(),
        ],
        &[&seeds],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_is_log_time_times_principal() {
        assert_eq!(vote_weight(10, 0), 0);
        assert_eq!(vote_weight(10, 1), 0); // age < 2 -> no weight
        assert_eq!(vote_weight(10, 4), 20); // floor(log2(4))=2 * 10
        assert_eq!(vote_weight(10, 1024), 100); // floor(log2(1024))=10 * 10
        assert_eq!(vote_weight(0, 1024), 0);
    }

    #[test]
    fn state_round_trips() {
        let sub_program = Pubkey::new_unique();
        let sub_pool = Pubkey::new_unique();
        let c = Config {
            coin_mint: Pubkey::new_unique(),
            distribution_program: Pubkey::new_unique(),
            distribution_config: Pubkey::new_unique(),
            subledger_program: sub_program,
            subledger_pool: sub_pool,
            _reserved: Pubkey::default(),
            total_voted_principal: 7,
            total_cast_weight: 70,
            outstanding_principal: 12,
            bump: 250,
            bootstrap_end_slot: 1234,
            bootstrap_delay_slots: 100,
            bootstrap_start_slot: 1134,
        };
        let mut b = [0u8; CONFIG_SIZE];
        c.serialize(&mut b);
        let d = Config::deserialize(&b).unwrap();
        assert_eq!(d.total_voted_principal, 7);
        assert_eq!(d.outstanding_principal, 12);
        assert_eq!(d.bump, 250);
        assert_eq!(d.bootstrap_end_slot, 1234);
        assert_eq!(d.bootstrap_delay_slots, 100);
        assert_eq!(d.bootstrap_start_slot, 1134);
        assert_eq!(d.subledger_program, sub_program);
        assert_eq!(d.subledger_pool, sub_pool);

        let p = Ballot {
            owner: Pubkey::new_unique(),
            voted_proposal: Pubkey::new_unique(),
            voted_weight: 40,
            voted_principal: 5,
        };
        let mut pb = [0u8; BALLOT_SIZE];
        p.serialize(&mut pb);
        let dp = Ballot::deserialize(&pb).unwrap();
        assert_eq!(dp.voted_principal, 5);
        assert!(dp.has_live_ballot());

        let pv = ProposalVote {
            config: Pubkey::new_unique(),
            distribution_proposal: Pubkey::new_unique(),
            support_weight: 8,
            support_principal: 2,
            executed: true,
            snapshot_entry_count: 7,
            snapshot_total_amount: 4242,
        };
        let mut vb = [0u8; PROPOSAL_SIZE];
        pv.serialize(&mut vb);
        let dv = ProposalVote::deserialize(&vb).unwrap();
        assert_eq!(dv.support_weight, 8);
        assert!(dv.executed);
        assert_eq!(dv.snapshot_entry_count, 7);
        assert_eq!(dv.snapshot_total_amount, 4242);
    }
}
