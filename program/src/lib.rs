//! Rewards program: staking vault + COIN rewards for insurance depositors and LPs.
//! Non-upgradeable. No admin keys. CoinConfig authority gates market registration.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

#[allow(unused_imports)]
use alloc::format; // Required by entrypoint! macro in SBF builds

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    declare_id,
    entrypoint,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::{clock::Clock, Sysvar},
};

declare_id!("Rewards111111111111111111111111111111111111");

// Re-export percolator-prog types we need
use percolator_prog::constants::ENGINE_OFF;
use percolator_prog::state;

/// Fixed-point scale for reward math.
pub const FP: u128 = 1u128 << 64;

/// Hard cap on K to bound COIN inflation from LP rewards.
pub const MAX_LP_COIN_PER_FEE_FP: u128 = 1_000_000u128 << 64; // 1M COIN per fee-atom max

/// Instruction tags
const IX_INIT_MARKET_REWARDS: u8 = 0;
const IX_STAKE: u8 = 1;
const IX_UNSTAKE: u8 = 2;
const IX_INIT_COIN_CONFIG: u8 = 3;
const IX_CLAIM_STAKE_REWARDS: u8 = 4;
const IX_CLAIM_LP_REWARDS: u8 = 5;

// ============================================================================
// Account sizes
// ============================================================================

/// MarketRewardsCfg: 8 + 32 + 32 + 32 + 8 + 16 + 8 + 8 + 16 + 8 + 8 = 176
const MRC_SIZE: usize = 8 + 32 + 32 + 32 + 8 + 16 + 8 + 8 + 16 + 8 + 8;
/// StakePosition: 8 + 8 + 8 + 16 + 8 = 48
const SP_SIZE: usize = 8 + 8 + 8 + 16 + 8;
/// LpClaimState: 8 + 32 = 40 (u256 = 32 bytes)
const LCS_SIZE: usize = 8 + 32;
/// CoinConfig: 8 + 32 = 40
const COIN_CFG_SIZE: usize = 8 + 32;

// Discriminators
const MRC_DISC: [u8; 8] = *b"MRC_V002";
const SP_DISC: [u8; 8] = *b"SP__INIT";
const LCS_DISC: [u8; 8] = *b"LCS_INIT";
const COIN_CFG_DISC: [u8; 8] = *b"CCFG_INI";

// ============================================================================
// PDA seeds
// ============================================================================

fn mrc_seeds(market_slab: &Pubkey) -> [&[u8]; 2] {
    [b"mrc", market_slab.as_ref()]
}

fn sp_seeds<'a>(market_slab: &'a Pubkey, user: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"sp", market_slab.as_ref(), user.as_ref()]
}

fn mint_authority_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_mint_authority", coin_mint.as_ref()]
}

fn coin_cfg_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_cfg", coin_mint.as_ref()]
}

fn stake_vault_seeds(market_slab: &Pubkey) -> [&[u8]; 2] {
    [b"stake_vault", market_slab.as_ref()]
}

// ============================================================================
// Instruction deserialization
// ============================================================================

fn read_u8(data: &mut &[u8]) -> Result<u8, ProgramError> {
    if data.is_empty() { return Err(ProgramError::InvalidInstructionData); }
    let val = data[0];
    *data = &data[1..];
    Ok(val)
}

fn read_u16(data: &mut &[u8]) -> Result<u16, ProgramError> {
    if data.len() < 2 { return Err(ProgramError::InvalidInstructionData); }
    let val = u16::from_le_bytes([data[0], data[1]]);
    *data = &data[2..];
    Ok(val)
}

fn read_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 8 { return Err(ProgramError::InvalidInstructionData); }
    let val = u64::from_le_bytes(data[..8].try_into().unwrap());
    *data = &data[8..];
    Ok(val)
}

fn read_u128(data: &mut &[u8]) -> Result<u128, ProgramError> {
    if data.len() < 16 { return Err(ProgramError::InvalidInstructionData); }
    let val = u128::from_le_bytes(data[..16].try_into().unwrap());
    *data = &data[16..];
    Ok(val)
}

// ============================================================================
// CoinConfig — shared across all markets using the same COIN mint
// ============================================================================

struct CoinConfig {
    authority: Pubkey,
}

impl CoinConfig {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < COIN_CFG_SIZE { return Err(ProgramError::InvalidAccountData); }
        if data[..8] != COIN_CFG_DISC { return Err(ProgramError::InvalidAccountData); }
        let authority = Pubkey::new_from_array(data[8..40].try_into().unwrap());
        Ok(Self { authority })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&COIN_CFG_DISC);
        data[8..40].copy_from_slice(self.authority.as_ref());
    }
}

// ============================================================================
// MarketRewardsCfg — per-market staking and reward configuration
// ============================================================================

struct MarketRewardsCfg {
    market_slab: Pubkey,        // [8..40]
    coin_mint: Pubkey,          // [40..72]
    collateral_mint: Pubkey,    // [72..104]
    n_per_epoch: u64,           // [104..112] COIN emitted per epoch to stakers
    k: u128,                    // [112..128] LP COIN per fee-atom (FP)
    epoch_slots: u64,           // [128..136] minimum lockup / reward period
    market_start_slot: u64,     // [136..144] from slab
    reward_per_token_stored: u128, // [144..160] accumulator (FP)
    last_update_slot: u64,      // [160..168]
    total_staked: u64,          // [168..176]
}

impl MarketRewardsCfg {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < MRC_SIZE { return Err(ProgramError::InvalidAccountData); }
        if data[..8] != MRC_DISC { return Err(ProgramError::InvalidAccountData); }
        let mut off = 8;
        let market_slab = Pubkey::new_from_array(data[off..off+32].try_into().unwrap()); off += 32;
        let coin_mint = Pubkey::new_from_array(data[off..off+32].try_into().unwrap()); off += 32;
        let collateral_mint = Pubkey::new_from_array(data[off..off+32].try_into().unwrap()); off += 32;
        let n_per_epoch = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let k = u128::from_le_bytes(data[off..off+16].try_into().unwrap()); off += 16;
        let epoch_slots = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let market_start_slot = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let reward_per_token_stored = u128::from_le_bytes(data[off..off+16].try_into().unwrap()); off += 16;
        let last_update_slot = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let total_staked = u64::from_le_bytes(data[off..off+8].try_into().unwrap());
        Ok(Self { market_slab, coin_mint, collateral_mint, n_per_epoch, k, epoch_slots,
                  market_start_slot, reward_per_token_stored, last_update_slot, total_staked })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&MRC_DISC);
        let mut off = 8;
        data[off..off+32].copy_from_slice(self.market_slab.as_ref()); off += 32;
        data[off..off+32].copy_from_slice(self.coin_mint.as_ref()); off += 32;
        data[off..off+32].copy_from_slice(self.collateral_mint.as_ref()); off += 32;
        data[off..off+8].copy_from_slice(&self.n_per_epoch.to_le_bytes()); off += 8;
        data[off..off+16].copy_from_slice(&self.k.to_le_bytes()); off += 16;
        data[off..off+8].copy_from_slice(&self.epoch_slots.to_le_bytes()); off += 8;
        data[off..off+8].copy_from_slice(&self.market_start_slot.to_le_bytes()); off += 8;
        data[off..off+16].copy_from_slice(&self.reward_per_token_stored.to_le_bytes()); off += 16;
        data[off..off+8].copy_from_slice(&self.last_update_slot.to_le_bytes()); off += 8;
        data[off..off+8].copy_from_slice(&self.total_staked.to_le_bytes());
    }
}

// ============================================================================
// StakePosition — per (market, user) staking state
// ============================================================================

struct StakePosition {
    amount: u64,                   // [8..16]
    deposit_slot: u64,             // [16..24]
    reward_per_token_paid: u128,   // [24..40]
    pending_rewards: u64,          // [40..48]
}

impl StakePosition {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < SP_SIZE { return Err(ProgramError::InvalidAccountData); }
        if data[..8] != SP_DISC { return Err(ProgramError::InvalidAccountData); }
        let amount = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let deposit_slot = u64::from_le_bytes(data[16..24].try_into().unwrap());
        let reward_per_token_paid = u128::from_le_bytes(data[24..40].try_into().unwrap());
        let pending_rewards = u64::from_le_bytes(data[40..48].try_into().unwrap());
        Ok(Self { amount, deposit_slot, reward_per_token_paid, pending_rewards })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&SP_DISC);
        data[8..16].copy_from_slice(&self.amount.to_le_bytes());
        data[16..24].copy_from_slice(&self.deposit_slot.to_le_bytes());
        data[24..40].copy_from_slice(&self.reward_per_token_paid.to_le_bytes());
        data[40..48].copy_from_slice(&self.pending_rewards.to_le_bytes());
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn create_pda_account<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    program_id: &Pubkey,
    seeds: &[&[u8]],
    size: usize,
) -> ProgramResult {
    let (expected, bump) = Pubkey::find_program_address(seeds, program_id);
    if *target.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(size);
    let mut seeds_with_bump: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::from(seeds);
    let bump_bytes = [bump];
    seeds_with_bump.push(&bump_bytes);
    invoke_signed(
        &system_instruction::create_account(payer.key, target.key, lamports, size as u64, program_id),
        &[payer.clone(), target.clone(), system_program.clone()],
        &[&seeds_with_bump],
    )
}

/// Mint COIN tokens via PDA authority.
fn mint_coin<'a>(
    token_program: &AccountInfo<'a>,
    coin_mint: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    mint_authority: &AccountInfo<'a>,
    amount: u64,
    signer_seeds: &[&[u8]],
) -> ProgramResult {
    if amount == 0 { return Ok(()); }
    let ix = spl_token::instruction::mint_to(
        token_program.key,
        coin_mint.key,
        destination.key,
        mint_authority.key,
        &[],
        amount,
    )?;
    invoke_signed(
        &ix,
        &[coin_mint.clone(), destination.clone(), mint_authority.clone(), token_program.clone()],
        &[signer_seeds],
    )
}

/// Update the reward accumulator in MRC. Returns updated reward_per_token.
fn update_accumulator(cfg: &mut MarketRewardsCfg, current_slot: u64) {
    if cfg.total_staked == 0 || current_slot <= cfg.last_update_slot || cfg.epoch_slots == 0 {
        cfg.last_update_slot = current_slot;
        return;
    }
    let elapsed = current_slot - cfg.last_update_slot;
    // delta = n_per_epoch * elapsed * FP / (epoch_slots * total_staked)
    // Use u256 intermediate to avoid overflow
    let n_elapsed = (cfg.n_per_epoch as u128).saturating_mul(elapsed as u128);
    let (num_lo, num_hi) = mul_u128_wide(n_elapsed, FP);
    let denom = (cfg.epoch_slots as u128).saturating_mul(cfg.total_staked as u128);
    if denom > 0 {
        let delta = div_u256_by_u128(num_lo, num_hi, denom);
        cfg.reward_per_token_stored = cfg.reward_per_token_stored.saturating_add(delta);
    }
    cfg.last_update_slot = current_slot;
}

/// Compute earned COIN for a position, add to pending.
fn settle_pending(pos: &mut StakePosition, reward_per_token: u128) {
    if pos.amount == 0 { return; }
    let delta = reward_per_token.saturating_sub(pos.reward_per_token_paid);
    let (lo, hi) = mul_u128_wide(pos.amount as u128, delta);
    // Divide by FP (>> 64)
    let earned_u128 = (lo >> 64) | (hi << 64);
    let earned = core::cmp::min(earned_u128, u64::MAX as u128) as u64;
    pos.pending_rewards = pos.pending_rewards.saturating_add(earned);
    pos.reward_per_token_paid = reward_per_token;
}

/// Read LP owner pubkey from slab account data.
fn read_lp_owner_from_slab(slab_data: &[u8], lp_idx: u16) -> Result<Pubkey, ProgramError> {
    let engine_data = &slab_data[ENGINE_OFF..];
    let acct = read_account_from_engine(engine_data, lp_idx)?;
    if acct.kind != 1 {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(Pubkey::new_from_array(acct.owner))
}

struct AccountSlice {
    kind: u8,
    owner: [u8; 32],
    #[allow(dead_code)]
    fees_earned_total: u128,
}

fn read_account_from_engine(engine_data: &[u8], idx: u16) -> Result<AccountSlice, ProgramError> {
    use core::mem::size_of;
    let account_size: usize = size_of::<percolator::Account>();
    let engine_size = size_of::<percolator::RiskEngine>();
    let max_accounts = percolator::MAX_ACCOUNTS;
    let accounts_offset = engine_size - max_accounts * account_size;

    let acct_start = accounts_offset + (idx as usize) * account_size;
    let acct_end = acct_start + account_size;
    if acct_end > engine_data.len() {
        return Err(ProgramError::InvalidAccountData);
    }

    let acct_data = &engine_data[acct_start..acct_end];
    let kind = acct_data[24];
    let owner_off = 8 + 16 + 1 + 7 + 16 + 8 + 8 + 16 + 16 + 8 + 16 + 32 + 32;
    let owner: [u8; 32] = acct_data[owner_off..owner_off+32].try_into().unwrap();
    let fees_off = owner_off + 32 + 16 + 8;
    let fees_earned_total = u128::from_le_bytes(acct_data[fees_off..fees_off+16].try_into().unwrap());

    Ok(AccountSlice { kind, owner, fees_earned_total })
}


// ============================================================================
// Entrypoint
// ============================================================================

entrypoint!(process_instruction);

pub fn process_instruction<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    instruction_data: &[u8],
) -> ProgramResult {
    let mut data = instruction_data;
    let tag = read_u8(&mut data)?;

    match tag {
        IX_INIT_MARKET_REWARDS => process_init_market_rewards(program_id, accounts, &mut data),
        IX_STAKE => process_stake(program_id, accounts, &mut data),
        IX_UNSTAKE => process_unstake(program_id, accounts, &mut data),
        IX_INIT_COIN_CONFIG => process_init_coin_config(program_id, accounts, &mut data),
        IX_CLAIM_STAKE_REWARDS => process_claim_stake_rewards(program_id, accounts),
        IX_CLAIM_LP_REWARDS => process_claim_lp_rewards(program_id, accounts, &mut data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ============================================================================
// init_coin_config
// ============================================================================
// Accounts:
//   [0] authority (signer, writable)
//   [1] coin_mint (read-only)
//   [2] coin_config PDA (writable, to create)
//   [3] system_program

fn process_init_coin_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    _data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Validate coin_mint: freeze_authority must be None, mint_authority must be our PDA
    let mint_data = coin_mint.try_borrow_data()?;
    let mint_info = spl_token::state::Mint::unpack(&mint_data)?;
    if mint_info.freeze_authority.is_some() {
        msg!("COIN mint must have freeze_authority = None");
        return Err(ProgramError::InvalidAccountData);
    }
    let (expected_mint_auth, _) = Pubkey::find_program_address(
        &mint_authority_seeds(coin_mint.key),
        program_id,
    );
    match mint_info.mint_authority {
        solana_program::program_option::COption::Some(auth) if auth == expected_mint_auth => {}
        _ => {
            msg!("COIN mint_authority must be the rewards PDA");
            return Err(ProgramError::InvalidAccountData);
        }
    }
    drop(mint_data);

    // Create CoinConfig PDA (init guard)
    let seeds = coin_cfg_seeds(coin_mint.key);
    create_pda_account(authority, coin_cfg_account, system_program, program_id, &seeds, COIN_CFG_SIZE)?;

    let mut cfg_data = coin_cfg_account.try_borrow_mut_data()?;
    let cfg = CoinConfig { authority: *authority.key };
    cfg.serialize(&mut cfg_data);

    Ok(())
}

// ============================================================================
// init_market_rewards
// ============================================================================
// Accounts:
//   [0] authority (signer, writable — must match CoinConfig.authority)
//   [1] market_slab (read-only)
//   [2] mrc PDA (writable, to create)
//   [3] coin_mint (read-only)
//   [4] coin_config PDA (read-only)
//   [5] collateral_mint (read-only)
//   [6] stake_vault PDA (writable, to create — SPL token account)
//   [7] token_program
//   [8] rent sysvar
//   [9] system_program
//
// Data: N (u64), K (u128), epoch_slots (u64)

fn process_init_market_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let n_per_epoch = read_u64(data)?;
    let k = read_u128(data)?;
    let epoch_slots = read_u64(data)?;

    if k > MAX_LP_COIN_PER_FEE_FP {
        msg!("K exceeds MAX_LP_COIN_PER_FEE_FP");
        return Err(ProgramError::InvalidInstructionData);
    }
    if epoch_slots == 0 {
        msg!("epoch_slots must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    // Verify CoinConfig PDA and authority
    let (expected_cfg, _) = Pubkey::find_program_address(&coin_cfg_seeds(coin_mint.key), program_id);
    if *coin_cfg_account.key != expected_cfg { return Err(ProgramError::InvalidSeeds); }
    if coin_cfg_account.owner != program_id { return Err(ProgramError::IllegalOwner); }

    let cfg_data = coin_cfg_account.try_borrow_data()?;
    let coin_cfg = CoinConfig::deserialize(&cfg_data)?;
    drop(cfg_data);

    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Read market_start_slot from slab
    let slab_data = market_slab.try_borrow_data()?;
    let market_start_slot = state::read_market_start_slot(&slab_data);
    if market_start_slot == 0 {
        msg!("market_start_slot is 0; slab not initialized via futarchy flow");
        return Err(ProgramError::InvalidAccountData);
    }
    drop(slab_data);

    // Create MarketRewardsCfg PDA (init guard)
    let seeds = mrc_seeds(market_slab.key);
    create_pda_account(authority, mrc_account, system_program, program_id, &seeds, MRC_SIZE)?;

    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let cfg = MarketRewardsCfg {
        market_slab: *market_slab.key,
        coin_mint: *coin_mint.key,
        collateral_mint: *collateral_mint.key,
        n_per_epoch,
        k,
        epoch_slots,
        market_start_slot,
        reward_per_token_stored: 0,
        last_update_slot: market_start_slot,
        total_staked: 0,
    };
    cfg.serialize(&mut mrc_data);
    drop(mrc_data);

    // Create stake vault — SPL token account PDA
    let vault_seeds = stake_vault_seeds(market_slab.key);
    let (expected_vault, vault_bump) = Pubkey::find_program_address(&vault_seeds, program_id);
    if *stake_vault.key != expected_vault { return Err(ProgramError::InvalidSeeds); }

    let vault_signer_seeds: [&[u8]; 3] = [b"stake_vault", market_slab.key.as_ref(), &[vault_bump]];
    let rent = Rent::get()?;
    invoke_signed(
        &system_instruction::create_account(
            authority.key,
            stake_vault.key,
            rent.minimum_balance(spl_token::state::Account::LEN),
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        &[authority.clone(), stake_vault.clone(), system_program.clone()],
        &[&vault_signer_seeds],
    )?;

    // Initialize as token account — vault authority is the MRC PDA
    let (mrc_key, _) = Pubkey::find_program_address(&mrc_seeds(market_slab.key), program_id);
    let init_ix = spl_token::instruction::initialize_account2(
        &spl_token::ID,
        stake_vault.key,
        collateral_mint.key,
        &mrc_key,
    )?;
    invoke(
        &init_ix,
        &[stake_vault.clone(), collateral_mint.clone(), rent_sysvar.clone(), token_program.clone()],
    )?;

    Ok(())
}

// ============================================================================
// stake
// ============================================================================
// Accounts:
//   [0] user (signer)
//   [1] mrc PDA (writable)
//   [2] market_slab (read-only)
//   [3] user_collateral_ata (writable)
//   [4] stake_vault (writable)
//   [5] stake_position PDA (writable)
//   [6] token_program
//   [7] system_program
//   [8] clock
//
// Data: amount (u64)

fn process_stake<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let sp_account = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 { return Err(ProgramError::InvalidInstructionData); }
    if !user.is_signer { return Err(ProgramError::MissingRequiredSignature); }

    // Read and verify MRC
    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let mut cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc { return Err(ProgramError::InvalidSeeds); }
    if mrc_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    if *market_slab.key != cfg.market_slab { return Err(ProgramError::InvalidAccountData); }

    // Verify stake vault
    let (expected_vault, _) = Pubkey::find_program_address(&stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault { return Err(ProgramError::InvalidSeeds); }

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);

    // Load or create StakePosition
    let sp_seeds_arr = sp_seeds(&cfg.market_slab, user.key);
    let (expected_sp, _) = Pubkey::find_program_address(&sp_seeds_arr, program_id);
    if *sp_account.key != expected_sp { return Err(ProgramError::InvalidSeeds); }

    let mut pos = if sp_account.data_len() == 0 {
        // First stake — create PDA
        drop(mrc_data); // release borrow for CPI
        create_pda_account(user, sp_account, system_program, program_id, &sp_seeds_arr, SP_SIZE)?;
        mrc_data = mrc_account.try_borrow_mut_data()?;
        let mut sp_data = sp_account.try_borrow_mut_data()?;
        sp_data[..8].copy_from_slice(&SP_DISC);
        sp_data[8..SP_SIZE].fill(0);
        drop(sp_data);
        StakePosition { amount: 0, deposit_slot: 0, reward_per_token_paid: 0, pending_rewards: 0 }
    } else {
        if sp_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
        let sp_data = sp_account.try_borrow_data()?;
        let p = StakePosition::deserialize(&sp_data)?;
        drop(sp_data);
        p
    };

    // Settle pending rewards before changing position
    settle_pending(&mut pos, cfg.reward_per_token_stored);

    // Update MRC total_staked and serialize before CPI (preserves accumulator update)
    cfg.total_staked = cfg.total_staked.checked_add(amount).ok_or(ProgramError::ArithmeticOverflow)?;
    cfg.serialize(&mut mrc_data);

    // Transfer collateral from user to vault
    let xfer_ix = spl_token::instruction::transfer(
        token_program.key, user_ata.key, stake_vault.key, user.key, &[], amount,
    )?;
    drop(mrc_data); // release borrow for CPI
    invoke(&xfer_ix, &[user_ata.clone(), stake_vault.clone(), user.clone(), token_program.clone()])?;

    // Update position
    pos.amount = pos.amount.checked_add(amount).ok_or(ProgramError::ArithmeticOverflow)?;
    pos.deposit_slot = clock.slot; // reset lockup
    pos.reward_per_token_paid = cfg.reward_per_token_stored;

    // Write position
    let mut sp_data = sp_account.try_borrow_mut_data()?;
    pos.serialize(&mut sp_data);

    Ok(())
}

// ============================================================================
// unstake — withdraw collateral + claim pending COIN rewards
// ============================================================================
// Accounts:
//   [0] user (signer, writable — receives rent on close)
//   [1] mrc PDA (writable)
//   [2] market_slab (read-only)
//   [3] user_collateral_ata (writable)
//   [4] stake_vault (writable)
//   [5] stake_position PDA (writable)
//   [6] coin_mint (writable)
//   [7] user_coin_ata (writable)
//   [8] mint_authority PDA (read-only)
//   [9] token_program
//   [10] clock
//
// Data: amount (u64)

fn process_unstake<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let user_ata = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let sp_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let user_coin_ata = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 { return Err(ProgramError::InvalidInstructionData); }
    if !user.is_signer { return Err(ProgramError::MissingRequiredSignature); }

    // Read and verify MRC
    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let mut cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc { return Err(ProgramError::InvalidSeeds); }
    if mrc_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    if *market_slab.key != cfg.market_slab { return Err(ProgramError::InvalidAccountData); }

    // Verify stake vault PDA
    let (expected_vault, _) = Pubkey::find_program_address(&stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault { return Err(ProgramError::InvalidSeeds); }

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);

    // Load StakePosition
    if sp_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    let sp_data_r = sp_account.try_borrow_data()?;
    let mut pos = StakePosition::deserialize(&sp_data_r)?;
    drop(sp_data_r);

    if amount > pos.amount {
        msg!("Unstake amount exceeds staked balance");
        return Err(ProgramError::InsufficientFunds);
    }

    // Check lockup
    if clock.slot < pos.deposit_slot.saturating_add(cfg.epoch_slots) {
        msg!("Lockup period not elapsed");
        return Err(ProgramError::Custom(100)); // lockup not met
    }

    // Settle pending rewards
    settle_pending(&mut pos, cfg.reward_per_token_stored);

    // Update MRC total_staked and serialize before CPI (so re-reads see updated state)
    cfg.total_staked = cfg.total_staked.saturating_sub(amount);
    cfg.serialize(&mut mrc_data);

    // Transfer collateral from vault to user (signed by MRC PDA)
    let mrc_seeds_arr = mrc_seeds(&cfg.market_slab);
    let (_, mrc_bump) = Pubkey::find_program_address(&mrc_seeds_arr, program_id);
    let mrc_signer: [&[u8]; 3] = [b"mrc", cfg.market_slab.as_ref(), &[mrc_bump]];

    let xfer_ix = spl_token::instruction::transfer(
        token_program.key, stake_vault.key, user_ata.key, mrc_account.key, &[], amount,
    )?;
    drop(mrc_data); // release for CPI
    invoke_signed(
        &xfer_ix,
        &[stake_vault.clone(), user_ata.clone(), mrc_account.clone(), token_program.clone()],
        &[&mrc_signer],
    )?;

    // Mint pending COIN rewards
    let pending = pos.pending_rewards;
    if pending > 0 {
        if *coin_mint.key != cfg.coin_mint { return Err(ProgramError::InvalidAccountData); }
        let ma_seeds = mint_authority_seeds(&cfg.coin_mint);
        let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
        if *mint_authority.key != expected_ma { return Err(ProgramError::InvalidSeeds); }
        let bump_bytes = [ma_bump];
        let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", cfg.coin_mint.as_ref(), &bump_bytes];
        mint_coin(token_program, coin_mint, user_coin_ata, mint_authority, pending, &signer_seeds)?;
    }

    // Update position
    pos.amount -= amount;
    pos.pending_rewards = 0;

    if pos.amount == 0 {
        // Close position — return rent to user
        let dest_lamports = user.lamports();
        **user.try_borrow_mut_lamports()? = dest_lamports
            .checked_add(sp_account.lamports())
            .ok_or(ProgramError::ArithmeticOverflow)?;
        **sp_account.try_borrow_mut_lamports()? = 0;
        let mut sp_data = sp_account.try_borrow_mut_data()?;
        sp_data.fill(0);
    } else {
        let mut sp_data = sp_account.try_borrow_mut_data()?;
        pos.serialize(&mut sp_data);
    }

    Ok(())
}

// ============================================================================
// claim_stake_rewards — claim COIN without unstaking
// ============================================================================
// Accounts:
//   [0] user (signer)
//   [1] mrc PDA (writable)
//   [2] market_slab (read-only)
//   [3] stake_position PDA (writable)
//   [4] coin_mint (writable)
//   [5] user_coin_ata (writable)
//   [6] mint_authority PDA (read-only)
//   [7] token_program
//   [8] clock

fn process_claim_stake_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let sp_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let user_coin_ata = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    if !user.is_signer { return Err(ProgramError::MissingRequiredSignature); }

    // Read and verify MRC
    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let mut cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc { return Err(ProgramError::InvalidSeeds); }
    if mrc_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    if *market_slab.key != cfg.market_slab { return Err(ProgramError::InvalidAccountData); }

    // Verify StakePosition PDA
    let sp_seeds_arr = sp_seeds(&cfg.market_slab, user.key);
    let (expected_sp, _) = Pubkey::find_program_address(&sp_seeds_arr, program_id);
    if *sp_account.key != expected_sp { return Err(ProgramError::InvalidSeeds); }
    if sp_account.owner != program_id { return Err(ProgramError::IllegalOwner); }

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);
    cfg.serialize(&mut mrc_data);
    drop(mrc_data);

    // Load position, settle, mint
    let sp_data_r = sp_account.try_borrow_data()?;
    let mut pos = StakePosition::deserialize(&sp_data_r)?;
    drop(sp_data_r);

    settle_pending(&mut pos, cfg.reward_per_token_stored);

    let pending = pos.pending_rewards;
    if pending > 0 {
        if *coin_mint.key != cfg.coin_mint { return Err(ProgramError::InvalidAccountData); }
        let ma_seeds = mint_authority_seeds(&cfg.coin_mint);
        let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
        if *mint_authority.key != expected_ma { return Err(ProgramError::InvalidSeeds); }
        let bump_bytes = [ma_bump];
        let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", cfg.coin_mint.as_ref(), &bump_bytes];
        mint_coin(token_program, coin_mint, user_coin_ata, mint_authority, pending, &signer_seeds)?;
        pos.pending_rewards = 0;
    }

    let mut sp_data = sp_account.try_borrow_mut_data()?;
    pos.serialize(&mut sp_data);

    Ok(())
}

// ============================================================================
// claim_lp_rewards
// ============================================================================
// Accounts:
//   [0] lp_owner (signer)
//   [1] mrc PDA (read-only)
//   [2] market_slab (read-only)
//   [3] lp_claim_state PDA (writable)
//   [4] coin_mint (writable)
//   [5] coin_ata (writable)
//   [6] mint_authority PDA (read-only)
//   [7] token_program
//   [8] percolator_program
//   [9] system_program
//
// Data: lp_idx (u16)

fn process_claim_lp_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let lp_owner = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let lcs_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_ata = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let lp_idx = read_u16(data)?;

    if !lp_owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Read MarketRewardsCfg
    let mrc_data = mrc_account.try_borrow_data()?;
    let cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    drop(mrc_data);

    // Verify MRC PDA
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc { return Err(ProgramError::InvalidSeeds); }
    if mrc_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    if *market_slab.key != cfg.market_slab { return Err(ProgramError::InvalidAccountData); }
    if market_slab.owner != percolator_program.key {
        msg!("market_slab owner does not match percolator_program");
        return Err(ProgramError::IllegalOwner);
    }

    // Verify signer is the LP position's owner
    let slab_data = market_slab.try_borrow_data()?;
    let lp_owner_pubkey = read_lp_owner_from_slab(&slab_data, lp_idx)?;
    drop(slab_data);

    if *lp_owner.key != lp_owner_pubkey {
        msg!("Signer does not match LP position owner");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // CPI to percolator::QueryLpFees
    let query_ix_data = {
        let mut d = alloc::vec![24u8]; // tag = 24
        d.extend_from_slice(&lp_idx.to_le_bytes());
        d
    };
    let query_ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*market_slab.key, false),
        ],
        data: query_ix_data,
    };
    invoke(&query_ix, &[market_slab.clone(), percolator_program.clone()])?;

    let (returning_program, return_data) = solana_program::program::get_return_data()
        .ok_or(ProgramError::InvalidAccountData)?;
    if returning_program != *percolator_program.key { return Err(ProgramError::InvalidAccountData); }
    if return_data.len() < 16 { return Err(ProgramError::InvalidAccountData); }
    let fees_earned_total = u128::from_le_bytes(return_data[..16].try_into().unwrap());

    // LP reward math (§8.3):
    let (entitled_lo, entitled_hi) = mul_u128_wide(fees_earned_total, cfg.k);

    let lp_idx_bytes = lp_idx.to_le_bytes();
    let lcs_seeds_arr: [&[u8]; 3] = [b"lcs", cfg.market_slab.as_ref(), &lp_idx_bytes];
    let (expected_lcs, _) = Pubkey::find_program_address(&lcs_seeds_arr, program_id);
    if *lcs_account.key != expected_lcs { return Err(ProgramError::InvalidSeeds); }

    let (claimed_lo, claimed_hi): (u128, u128);
    if lcs_account.data_len() == 0 {
        create_pda_account(lp_owner, lcs_account, system_program, program_id, &lcs_seeds_arr, LCS_SIZE)?;
        let mut lcs_data = lcs_account.try_borrow_mut_data()?;
        lcs_data[..8].copy_from_slice(&LCS_DISC);
        lcs_data[8..40].copy_from_slice(&[0u8; 32]);
        drop(lcs_data);
        claimed_lo = 0;
        claimed_hi = 0;
    } else {
        if lcs_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
        let lcs_data = lcs_account.try_borrow_data()?;
        if lcs_data.len() < LCS_SIZE || lcs_data[..8] != LCS_DISC { return Err(ProgramError::InvalidAccountData); }
        claimed_lo = u128::from_le_bytes(lcs_data[8..24].try_into().unwrap());
        claimed_hi = u128::from_le_bytes(lcs_data[24..40].try_into().unwrap());
        drop(lcs_data);
    }

    let (claimable_lo, claimable_hi) = sub_u256(entitled_lo, entitled_hi, claimed_lo, claimed_hi);
    let claimable_coins_u128 = (claimable_lo >> 64) | (claimable_hi << 64);
    let claimable_coins: u64 = if claimable_coins_u128 > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    } else {
        claimable_coins_u128 as u64
    };

    if claimable_coins == 0 { return Ok(()); }

    if *coin_mint.key != cfg.coin_mint { return Err(ProgramError::InvalidAccountData); }

    let ma_seeds = mint_authority_seeds(&cfg.coin_mint);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma { return Err(ProgramError::InvalidSeeds); }
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", cfg.coin_mint.as_ref(), &bump_bytes];
    mint_coin(token_program, coin_mint, coin_ata, mint_authority, claimable_coins, &signer_seeds)?;

    // Update LpClaimState
    let added_lo = (claimable_coins as u128) << 64;
    let added_hi = (claimable_coins as u128) >> 64;
    let (new_claimed_lo, new_claimed_hi) = add_u256(claimed_lo, claimed_hi, added_lo, added_hi);

    let mut lcs_data = lcs_account.try_borrow_mut_data()?;
    lcs_data[8..24].copy_from_slice(&new_claimed_lo.to_le_bytes());
    lcs_data[24..40].copy_from_slice(&new_claimed_hi.to_le_bytes());

    Ok(())
}

// ============================================================================
// u256 arithmetic helpers
// ============================================================================

fn mul_u128_wide(a: u128, b: u128) -> (u128, u128) {
    let a_lo = a as u64 as u128;
    let a_hi = a >> 64;
    let b_lo = b as u64 as u128;
    let b_hi = b >> 64;

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    let mid = (ll >> 64) + (lh & 0xFFFF_FFFF_FFFF_FFFF) + (hl & 0xFFFF_FFFF_FFFF_FFFF);
    let lo = (ll & 0xFFFF_FFFF_FFFF_FFFF) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);

    (lo, hi)
}

fn sub_u256(a_lo: u128, a_hi: u128, b_lo: u128, b_hi: u128) -> (u128, u128) {
    if a_hi < b_hi || (a_hi == b_hi && a_lo < b_lo) {
        return (0, 0);
    }
    let (lo, borrow) = a_lo.overflowing_sub(b_lo);
    let hi = a_hi - b_hi - if borrow { 1 } else { 0 };
    (lo, hi)
}

fn add_u256(a_lo: u128, a_hi: u128, b_lo: u128, b_hi: u128) -> (u128, u128) {
    let (lo, carry) = a_lo.overflowing_add(b_lo);
    let hi = a_hi.saturating_add(b_hi).saturating_add(if carry { 1 } else { 0 });
    (lo, hi)
}

/// Divide a u256 (n_lo, n_hi) by a u128 divisor. Returns u128 (saturates on overflow).
fn div_u256_by_u128(n_lo: u128, n_hi: u128, d: u128) -> u128 {
    if d == 0 { return u128::MAX; }
    if n_hi == 0 { return n_lo / d; }
    if n_hi >= d { return u128::MAX; } // result would overflow u128

    // Long division: process n_lo bits from high to low.
    // After processing all of n_hi (which is < d), remainder = n_hi.
    let mut rem: u128 = n_hi;
    let mut quot: u128 = 0;

    for i in (0..128u32).rev() {
        let bit = (n_lo >> i) & 1;
        let overflow = rem >> 127 != 0;
        rem = rem.wrapping_shl(1) | bit;

        if overflow || rem >= d {
            rem = rem.wrapping_sub(d);
            quot |= 1u128 << i;
        }
    }

    quot
}
