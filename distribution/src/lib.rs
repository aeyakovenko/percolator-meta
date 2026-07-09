//! Genesis COIN distribution by on-chain proposal list + permissionless claim.
//!
//! A proposal is a single on-chain account grown in append-sized increments to hold up to ~10k
//! `(recipient pubkey, amount)` entries (40 bytes each → ~400KB). After a winner
//! is sealed by the configured `authority`, recipients **claim** their own entry
//! permissionlessly (pull model, indexed by offset); anything unclaimed when the
//! window closes is **burned**.
//!
//! The program distributes a *fixed, pre-existing* COIN supply held in a vault it
//! controls — it never mints. A bug here can at worst misallocate the fixed pool;
//! it cannot mint COIN or touch user funds elsewhere.
//!
//! ## Decider seam (this is the "proposal program"; the decider is pluggable)
//!
//! This program is intentionally **agnostic to *how* the winner is chosen**. The
//! sole trust hook is `config.authority` — the "decider" — which is bound into the
//! config-PDA seed (`["dist_config", coin_mint, authority]`, finding P/AA) so each
//! decider gets its own isolated config + vault. Any signer or program PDA can be a
//! decider; this program never references `genesis-vote`. Two interchangeable
//! deciders today:
//!   - **`genesis-vote`** (the default): log-time-weighted insurance quorum vote;
//!     its config PDA is the `authority` and its `trigger` CPIs `IX_SEAL_WINNER`.
//!   - **a deterministic points distributor** (e.g. residual-backing points): a
//!     program whose PDA is the `authority`, which *computes* the entry list from
//!     on-chain counters and seals it. Reuses this program **unchanged**.
//!
//! ### Decider contract (what any decider must do)
//!   1. Be `config.authority` — pass its key as `authority` at `IX_INIT_CONFIG`
//!      (the config PDA derives from it, so the decider is fixed at config creation).
//!   2. Produce a proposal: `IX_CREATE_PROPOSAL` then `IX_APPEND_ENTRIES`
//!      (creator-gated). A deterministic decider is itself the creator and writes
//!      `(recipient, amount)` straight from its counters; a vote decider lets
//!      candidates self-submit and only *picks* among them.
//!   3. `IX_SEAL_WINNER` **signed as `config.authority`** — for a program decider,
//!      via `invoke_signed` with its PDA seeds. Sealing is one-shot + irreversible.
//! Swapping vote → deterministic is purely a *setup* choice (which key is
//! `authority` at init + which program seals); no change to this program is needed.

#![no_std]
extern crate alloc;

#[allow(unused_imports)]
use alloc::format; // entrypoint!/msg! macro in SBF builds

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    declare_id,
    entrypoint::ProgramResult,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
    sysvar::Sysvar,
};

declare_id!("D1str1but1on11111111111111111111111111111111");

const CONFIG_DISC: [u8; 8] = *b"DISTCFG1";
const PROPOSAL_DISC: [u8; 8] = *b"DISTPRP1";
const CONFIG_SIZE: usize = 168;
const PROPOSAL_HEADER: usize = 104;
const ENTRY_SIZE: usize = 40; // pubkey(32) + amount(8)
const MAX_ENTRIES: u32 = 10_000;

const IX_INIT_CONFIG: u8 = 0;
const IX_CREATE_PROPOSAL: u8 = 1;
const IX_APPEND_ENTRIES: u8 = 2;
const IX_SEAL_WINNER: u8 = 3;
const IX_CLAIM: u8 = 4;
const IX_BURN_UNCLAIMED: u8 = 5;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

// The config PDA binds the AUTHORITY into its seed (finding P/AA), not just the coin_mint.
// Otherwise init_config was front-run squattable: an attacker could init the per-mint config FIRST
// with authority=themselves AND the deployer's already-funded vault (owned by the deterministic
// PDA) — then seal a self-dealing proposal and CLAIM the entire COIN supply (theft). By folding the
// authority into the seed, an attacker's authority lands at a DIFFERENT PDA whose vault they must
// own + fund themselves (impossible without the COIN), so the legit (authority = gv config PDA)
// config + funded vault are untouchable.
fn config_seeds<'a>(coin_mint: &'a Pubkey, authority: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"dist_config", coin_mint.as_ref(), authority.as_ref()]
}

fn proposal_seeds<'a>(config: &'a Pubkey, id: &'a [u8; 8]) -> [&'a [u8]; 3] {
    [b"dist_proposal", config.as_ref(), id]
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct Config {
    coin_mint: Pubkey,
    vault: Pubkey,
    authority: Pubkey,
    claim_window_slots: u64,
    total_supply: u64,
    sealed_proposal: Pubkey, // default() = not yet sealed
    seal_slot: u64,
    bump: u8,
}

impl Config {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < CONFIG_SIZE || data[..8] != CONFIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            vault: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            authority: Pubkey::new_from_array(data[72..104].try_into().unwrap()),
            claim_window_slots: u64::from_le_bytes(data[104..112].try_into().unwrap()),
            total_supply: u64::from_le_bytes(data[112..120].try_into().unwrap()),
            sealed_proposal: Pubkey::new_from_array(data[120..152].try_into().unwrap()),
            seal_slot: u64::from_le_bytes(data[152..160].try_into().unwrap()),
            bump: data[160],
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&CONFIG_DISC);
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[40..72].copy_from_slice(self.vault.as_ref());
        data[72..104].copy_from_slice(self.authority.as_ref());
        data[104..112].copy_from_slice(&self.claim_window_slots.to_le_bytes());
        data[112..120].copy_from_slice(&self.total_supply.to_le_bytes());
        data[120..152].copy_from_slice(self.sealed_proposal.as_ref());
        data[152..160].copy_from_slice(&self.seal_slot.to_le_bytes());
        data[160] = self.bump;
        data[161..CONFIG_SIZE].fill(0);
    }

    fn is_sealed(&self) -> bool {
        self.sealed_proposal != Pubkey::default()
    }
}

// ---------------------------------------------------------------------------
// Proposal header (entries follow at PROPOSAL_HEADER)
// ---------------------------------------------------------------------------

struct ProposalHeader {
    config: Pubkey,
    proposal_id: u64,
    creator: Pubkey,
    capacity: u32,
    entry_count: u32,
    total_amount: u64,
    sealed: bool,
}

impl ProposalHeader {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < PROPOSAL_HEADER || data[..8] != PROPOSAL_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let sealed = data[96];
        if sealed > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            config: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            proposal_id: u64::from_le_bytes(data[40..48].try_into().unwrap()),
            creator: Pubkey::new_from_array(data[48..80].try_into().unwrap()),
            capacity: u32::from_le_bytes(data[80..84].try_into().unwrap()),
            entry_count: u32::from_le_bytes(data[84..88].try_into().unwrap()),
            total_amount: u64::from_le_bytes(data[88..96].try_into().unwrap()),
            sealed: sealed == 1,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&PROPOSAL_DISC);
        data[8..40].copy_from_slice(self.config.as_ref());
        data[40..48].copy_from_slice(&self.proposal_id.to_le_bytes());
        data[48..80].copy_from_slice(self.creator.as_ref());
        data[80..84].copy_from_slice(&self.capacity.to_le_bytes());
        data[84..88].copy_from_slice(&self.entry_count.to_le_bytes());
        data[88..96].copy_from_slice(&self.total_amount.to_le_bytes());
        data[96] = self.sealed as u8;
        data[97..PROPOSAL_HEADER].fill(0);
    }
}

fn entry_offset(index: u32) -> usize {
    PROPOSAL_HEADER + (index as usize) * ENTRY_SIZE
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
        IX_INIT_CONFIG => init_config(program_id, accounts, data),
        IX_CREATE_PROPOSAL => create_proposal(program_id, accounts, data),
        IX_APPEND_ENTRIES => append_entries(program_id, accounts, data),
        IX_SEAL_WINNER => seal_winner(program_id, accounts, data),
        IX_CLAIM => claim(program_id, accounts, data),
        IX_BURN_UNCLAIMED => burn_unclaimed(program_id, accounts, data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn take_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (h, t) = data.split_at(8);
    *data = t;
    Ok(u64::from_le_bytes(h.try_into().unwrap()))
}
fn take_u32(data: &mut &[u8]) -> Result<u32, ProgramError> {
    if data.len() < 4 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (h, t) = data.split_at(4);
    *data = t;
    Ok(u32::from_le_bytes(h.try_into().unwrap()))
}

fn token_balance(account: &AccountInfo, expected_mint: &Pubkey) -> Result<u64, ProgramError> {
    if account.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let st = spl_token::state::Account::unpack(&account.try_borrow_data()?)?;
    if st.mint != *expected_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(st.amount)
}

// Create a program-owned PDA, tolerating an attacker pre-funding the (deterministic) address.
// System `create_account` aborts with AccountAlreadyInUse on ANY pre-existing lamports, so a 1-
// lamport transfer to the address (no signature needed) would PERMANENTLY brick init — the lamports
// can never be swept from a system-owned PDA. Top up the rent shortfall (a plain transfer) then
// allocate + assign via invoke_signed; both only require the account to be data-empty + system-owned.
// Callers gate re-init on `data_len() != 0` (NOT lamports). (finding AI)
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
    )
}

// init_config accounts: [payer(s,w), coin_mint, config(pda,w), vault, authority, system]
// data: claim_window_slots(u64), total_supply(u64)
fn init_config(program_id: &Pubkey, accounts: &[AccountInfo], mut data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let claim_window_slots = take_u64(&mut data)?;
    let total_supply = take_u64(&mut data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if total_supply == 0 || claim_window_slots == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let (expected_config, bump) =
        Pubkey::find_program_address(&config_seeds(coin_mint.key, authority.key), program_id);
    if *config_account.key != expected_config {
        return Err(ProgramError::InvalidSeeds);
    }
    if config_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    // Fixed-supply invariant (README Safety §4): the COIN mint authority MUST be
    // revoked before a distribution can be created against it. Otherwise the mint
    // authority holder could mint unlimited COIN outside the fixed pool and dilute
    // every recipient's governance/value ("no mint to drain"). The freeze authority
    // must also be revoked, or it could freeze the vault (DOS all claims) or a
    // recipient's account. This makes the fixed pool the entire COIN supply, period.
    // Pack::unpack does NOT verify the owning program, so a non-SPL account with
    // token-shaped bytes would otherwise pass; require SPL Token ownership first
    // (same guard as token_balance, :234) to block a type-cosplay init squat.
    if coin_mint.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let mint = spl_token::state::Mint::unpack(&coin_mint.try_borrow_data()?)?;
    if mint.mint_authority.is_some() || mint.freeze_authority.is_some() {
        return Err(ProgramError::InvalidAccountData);
    }
    // ...and the mint's ENTIRE supply must equal the distributed pool. Revoking the
    // mint authority only stops FUTURE minting; without this an attacker could
    // pre-mint extra COIN to themselves before revoking and fund the vault with just
    // total_supply, holding undistributed COIN that dominates governance (the COIN IS
    // the MetaDAO). Combined with the vault-funding check below, this proves every
    // COIN that exists is in this distribution vault.
    if mint.supply != total_supply {
        return Err(ProgramError::InvalidAccountData);
    }

    // Vault is the COIN holding account, authority = config PDA. Require SPL Token
    // ownership before unpacking (see the mint check above) so a non-SPL token-shaped
    // account can't squat the canonical config PDA with an unusable vault.
    if vault.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let vault_state = spl_token::state::Account::unpack(&vault.try_borrow_data()?)?;
    if vault_state.mint != *coin_mint.key || vault_state.owner != expected_config {
        return Err(ProgramError::InvalidAccountData);
    }
    // Solvency invariant: the vault must already hold the full promised supply. The
    // seal only enforces `total_amount <= total_supply` (the claimed number), so a
    // config whose vault is underfunded would let early claimants drain it and
    // STRAND honest late claimants (a claim-race LOF). Tie the promised supply to
    // real tokens up front: a config can never promise more than the vault holds.
    if vault_state.amount < total_supply {
        return Err(ProgramError::InsufficientFunds);
    }

    let bump_arr = [bump];
    let seeds: [&[u8]; 4] = [b"dist_config", coin_mint.key.as_ref(), authority.key.as_ref(), &bump_arr];
    create_pda_robust(payer, config_account, system_program, program_id, &seeds, CONFIG_SIZE)?;

    let config = Config {
        coin_mint: *coin_mint.key,
        vault: *vault.key,
        authority: *authority.key,
        claim_window_slots,
        total_supply,
        sealed_proposal: Pubkey::default(),
        seal_slot: 0,
        bump,
    };
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// create_proposal accounts: [creator(s,w), config, proposal(pda,w), system]
// data: proposal_id(u64), capacity(u32)
fn create_proposal(program_id: &Pubkey, accounts: &[AccountInfo], mut data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let creator = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let proposal_id = take_u64(&mut data)?;
    let capacity = take_u32(&mut data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !creator.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if capacity == 0 || capacity > MAX_ENTRIES {
        return Err(ProgramError::InvalidInstructionData);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if config.is_sealed() {
        return Err(ProgramError::InvalidInstructionData); // decision already made
    }

    let id_bytes = proposal_id.to_le_bytes();
    let (expected, bump) =
        Pubkey::find_program_address(&proposal_seeds(config_account.key, &id_bytes), program_id);
    if *proposal_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if proposal_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }

    let entries_size = (capacity as usize)
        .checked_mul(ENTRY_SIZE)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let final_size = PROPOSAL_HEADER
        .checked_add(entries_size)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    // Inner instructions may increase account data by at most 10,240 bytes. Fund the PDA for
    // its declared final capacity now, allocate only its header, and let append_entries grow the
    // program-owned account in transaction-sized increments.
    let final_rent = solana_program::rent::Rent::get()?.minimum_balance(final_size);
    let current = proposal_account.lamports();
    if current < final_rent {
        invoke(
            &system_instruction::transfer(creator.key, proposal_account.key, final_rent - current),
            &[creator.clone(), proposal_account.clone(), system_program.clone()],
        )?;
    }
    let bump_arr = [bump];
    let seeds: [&[u8]; 4] = [b"dist_proposal", config_account.key.as_ref(), &id_bytes, &bump_arr];
    create_pda_robust(
        creator,
        proposal_account,
        system_program,
        program_id,
        &seeds,
        PROPOSAL_HEADER,
    )?;

    let header = ProposalHeader {
        config: *config_account.key,
        proposal_id,
        creator: *creator.key,
        capacity,
        entry_count: 0,
        total_amount: 0,
        sealed: false,
    };
    header.serialize(&mut proposal_account.try_borrow_mut_data()?);
    Ok(())
}

// append_entries accounts: [creator(s), config, proposal(w)]
// data: count(u32), then count * (pubkey[32], amount[u64])
fn append_entries(program_id: &Pubkey, accounts: &[AccountInfo], mut data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let creator = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;

    let count = take_u32(&mut data)?;
    if !creator.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if data.len() != (count as usize) * ENTRY_SIZE {
        return Err(ProgramError::InvalidInstructionData);
    }
    if config_account.owner != program_id || proposal_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    let mut header = ProposalHeader::deserialize(&proposal_account.try_borrow_data()?)?;
    if header.config != *config_account.key || header.creator != *creator.key {
        return Err(ProgramError::InvalidAccountData);
    }
    if header.sealed || config.is_sealed() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let first_entry = header.entry_count;
    let new_entry_count = first_entry
        .checked_add(count)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    if new_entry_count > header.capacity {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut new_total_amount = header.total_amount;
    for i in 0..count {
        let off = (i as usize) * ENTRY_SIZE;
        let pk = Pubkey::new_from_array(data[off..off + 32].try_into().unwrap());
        let amount = u64::from_le_bytes(data[off + 32..off + 40].try_into().unwrap());
        if amount == 0 || pk == Pubkey::default() {
            return Err(ProgramError::InvalidInstructionData);
        }
        new_total_amount = new_total_amount
            .checked_add(amount)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        if new_total_amount > config.total_supply {
            return Err(ProgramError::InvalidInstructionData);
        }
    }

    let required_len = entry_offset(new_entry_count);
    if proposal_account.data_len() < required_len {
        proposal_account.realloc(required_len, false)?;
    }
    let mut pd = proposal_account.try_borrow_mut_data()?;
    for i in 0..count {
        let off = (i as usize) * ENTRY_SIZE;
        let pk = Pubkey::new_from_array(data[off..off + 32].try_into().unwrap());
        let amount = u64::from_le_bytes(data[off + 32..off + 40].try_into().unwrap());
        let eo = entry_offset(first_entry + i);
        pd[eo..eo + 32].copy_from_slice(pk.as_ref());
        pd[eo + 32..eo + 40].copy_from_slice(&amount.to_le_bytes());
    }
    header.entry_count = new_entry_count;
    header.total_amount = new_total_amount;
    header.serialize(&mut pd);
    Ok(())
}

// seal_winner accounts: [authority(s), config(w), proposal(w)]
fn seal_winner(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let authority = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;

    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id || proposal_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if *authority.key != config.authority {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config.is_sealed() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut pd = proposal_account.try_borrow_mut_data()?;
    let mut header = ProposalHeader::deserialize(&pd)?;
    if header.config != *config_account.key || header.entry_count == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    if header.total_amount > config.total_supply {
        return Err(ProgramError::InvalidInstructionData);
    }

    let clock = Clock::get()?;
    config.sealed_proposal = *proposal_account.key;
    config.seal_slot = clock.slot;
    header.sealed = true;

    header.serialize(&mut pd);
    drop(pd);
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// claim accounts: [recipient(s), config, proposal(w), vault(w), recipient_ata(w), token_program]
// data: index(u32)
fn claim(program_id: &Pubkey, accounts: &[AccountInfo], mut data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let recipient = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let proposal_account = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let recipient_ata = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let index = take_u32(&mut data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !recipient.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id || proposal_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if !config.is_sealed() || config.sealed_proposal != *proposal_account.key {
        return Err(ProgramError::InvalidAccountData); // only the winning proposal pays
    }
    if *vault.key != config.vault {
        return Err(ProgramError::InvalidAccountData);
    }
    let clock = Clock::get()?;
    // saturating: an absurd claim_window_slots (near u64::MAX) must NOT overflow-revert here — a
    // checked_add error would fail EVERY claim AND (mirrored below) every burn, permanently freezing
    // the whole vault. Saturating to u64::MAX instead means an over-long window just keeps claims open
    // indefinitely (burn never fires) — graceful, never a brick. Normal windows are unaffected.
    let window_end = config.seal_slot.saturating_add(config.claim_window_slots);
    if clock.slot >= window_end {
        return Err(ProgramError::InvalidInstructionData); // window closed
    }

    let mut pd = proposal_account.try_borrow_mut_data()?;
    let header = ProposalHeader::deserialize(&pd)?;
    if index >= header.entry_count {
        return Err(ProgramError::InvalidInstructionData);
    }
    let eo = entry_offset(index);
    let pk = Pubkey::new_from_array(pd[eo..eo + 32].try_into().unwrap());
    let amount = u64::from_le_bytes(pd[eo + 32..eo + 40].try_into().unwrap());
    if pk != *recipient.key {
        return Err(ProgramError::IllegalOwner); // pull model: only the named recipient
    }
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData); // already claimed
    }

    let bump_arr = [config.bump];
    let seeds: [&[u8]; 4] = [b"dist_config", config.coin_mint.as_ref(), config.authority.as_ref(), &bump_arr];
    invoke_signed(
        &spl_token::instruction::transfer(
            token_program.key,
            vault.key,
            recipient_ata.key,
            config_account.key,
            &[],
            amount,
        )?,
        &[vault.clone(), recipient_ata.clone(), config_account.clone(), token_program.clone()],
        &[&seeds],
    )?;

    // Zero the entry so it cannot be re-claimed.
    pd[eo + 32..eo + 40].copy_from_slice(&0u64.to_le_bytes());
    Ok(())
}

// burn_unclaimed accounts: [cranker(s), config, vault(w), coin_mint(w), token_program]
fn burn_unclaimed(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !cranker.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if !config.is_sealed() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *vault.key != config.vault || *coin_mint.key != config.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    let clock = Clock::get()?;
    // saturating (mirror of claim): an absurd window must not overflow-revert — it would brick the
    // burn AND claim together. Saturated, an over-long window simply keeps burn closed forever (claims
    // stay open); normal windows are unaffected.
    let window_end = config.seal_slot.saturating_add(config.claim_window_slots);
    if clock.slot < window_end {
        return Err(ProgramError::InvalidInstructionData); // window still open
    }

    let remaining = token_balance(vault, &config.coin_mint)?;
    if remaining > 0 {
        let bump_arr = [config.bump];
        let seeds: [&[u8]; 4] = [b"dist_config", config.coin_mint.as_ref(), config.authority.as_ref(), &bump_arr];
        invoke_signed(
            &spl_token::instruction::burn(
                token_program.key,
                vault.key,
                coin_mint.key,
                config_account.key,
                &[],
                remaining,
            )?,
            &[vault.clone(), coin_mint.clone(), config_account.clone(), token_program.clone()],
            &[&seeds],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_offsets_are_packed() {
        assert_eq!(entry_offset(0), PROPOSAL_HEADER);
        assert_eq!(entry_offset(1), PROPOSAL_HEADER + 40);
        assert_eq!(entry_offset(9_999), PROPOSAL_HEADER + 9_999 * 40);
    }

    #[test]
    fn config_round_trips() {
        let c = Config {
            coin_mint: Pubkey::new_unique(),
            vault: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            claim_window_slots: 1000,
            total_supply: 42_000_000,
            sealed_proposal: Pubkey::default(),
            seal_slot: 0,
            bump: 251,
        };
        let mut b = [0u8; CONFIG_SIZE];
        c.serialize(&mut b);
        let d = Config::deserialize(&b).unwrap();
        assert_eq!(d.coin_mint, c.coin_mint);
        assert_eq!(d.total_supply, 42_000_000);
        assert!(!d.is_sealed());
        assert_eq!(d.bump, 251);
    }

    #[test]
    fn proposal_header_round_trips() {
        let h = ProposalHeader {
            config: Pubkey::new_unique(),
            proposal_id: 3,
            creator: Pubkey::new_unique(),
            capacity: 10_000,
            entry_count: 1234,
            total_amount: 999,
            sealed: true,
        };
        let mut b = [0u8; PROPOSAL_HEADER];
        h.serialize(&mut b);
        let d = ProposalHeader::deserialize(&b).unwrap();
        assert_eq!(d.proposal_id, 3);
        assert_eq!(d.capacity, 10_000);
        assert_eq!(d.entry_count, 1234);
        assert_eq!(d.total_amount, 999);
        assert!(d.sealed);
    }
}
