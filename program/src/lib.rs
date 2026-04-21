//! Insurance deposit program: users deposit collateral into per-market vaults
//! and earn COIN (DAO token) as yield. No lockup — withdraw anytime.
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

use governance_adapter::{authority_address as governance_authority_address, id as governance_program_id};
use percolator_prog::state;

/// Fixed-point scale for reward math.
pub const FP: u128 = 1u128 << 64;

/// Instruction tags
const IX_INIT_MARKET_REWARDS: u8 = 0;
const IX_STAKE: u8 = 1;
const IX_UNSTAKE: u8 = 2;
const IX_INIT_COIN_CONFIG: u8 = 3;
const IX_CLAIM_STAKE_REWARDS: u8 = 4;
const IX_DRAW_INSURANCE: u8 = 5;
/// Register the MRC PDA as the percolator market's insurance_operator.
/// Callable only by the current percolator admin before admin burn.
/// Uses invoke_signed with MRC seeds so the new authority (the PDA)
/// is treated as a signer by percolator's UpdateAuthority handler.
const IX_REGISTER_INSURANCE_OPERATOR: u8 = 6;
/// Pull tokens from the percolator market's insurance fund into our
/// stake_vault via WithdrawInsuranceLimited. MRC PDA must be the
/// registered insurance_operator. Permissionless keeper — anyone can
/// call it; destination is always the stake_vault, so only deposit-
/// facing instructions and draw_insurance can redistribute the pulled
/// funds.
const IX_PULL_INSURANCE: u8 = 7;

/// Percolator instruction tags we CPI into
const PERC_IX_TOP_UP_INSURANCE: u8 = 9;
const PERC_IX_UPDATE_AUTHORITY: u8 = 32;
const PERC_IX_WITHDRAW_INSURANCE_LIMITED: u8 = 23;
const PERC_AUTHORITY_INSURANCE_OPERATOR: u8 = 4;

// ============================================================================
// Account sizes
// ============================================================================

/// MarketRewardsCfg: 8 + 32 + 32 + 32 + 8 + 8 + 8 + 16 + 8 + 8 = 160
const MRC_SIZE: usize = 8 + 32 + 32 + 32 + 8 + 8 + 8 + 16 + 8 + 8;
/// StakePosition: 8 + 8 + 8 + 16 + 8 = 48
const SP_SIZE: usize = 8 + 8 + 8 + 16 + 8;
/// CoinConfig: 8 + 32 = 40
const COIN_CFG_SIZE: usize = 8 + 32;

// Discriminators
const MRC_DISC: [u8; 8] = *b"MRC_V003";
const SP_DISC: [u8; 8] = *b"SP__INIT";
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

fn read_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 8 { return Err(ProgramError::InvalidInstructionData); }
    let val = u64::from_le_bytes(data[..8].try_into().unwrap());
    *data = &data[8..];
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
    epoch_slots: u64,           // [112..120] minimum lockup / reward period
    market_start_slot: u64,     // [120..128] from slab
    reward_per_token_stored: u128, // [128..144] accumulator (FP)
    last_update_slot: u64,      // [144..152]
    total_staked: u64,          // [152..160]
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
        let epoch_slots = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let market_start_slot = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let reward_per_token_stored = u128::from_le_bytes(data[off..off+16].try_into().unwrap()); off += 16;
        let last_update_slot = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let total_staked = u64::from_le_bytes(data[off..off+8].try_into().unwrap());
        Ok(Self { market_slab, coin_mint, collateral_mint, n_per_epoch, epoch_slots,
                  market_start_slot, reward_per_token_stored, last_update_slot, total_staked })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&MRC_DISC);
        let mut off = 8;
        data[off..off+32].copy_from_slice(self.market_slab.as_ref()); off += 32;
        data[off..off+32].copy_from_slice(self.coin_mint.as_ref()); off += 32;
        data[off..off+32].copy_from_slice(self.collateral_mint.as_ref()); off += 32;
        data[off..off+8].copy_from_slice(&self.n_per_epoch.to_le_bytes()); off += 8;
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

fn verify_token_program(token_program: &AccountInfo) -> ProgramResult {
    if *token_program.key != spl_token::ID {
        msg!("Expected SPL Token program");
        return Err(ProgramError::IncorrectProgramId);
    }
    Ok(())
}

fn load_token_account(account: &AccountInfo) -> Result<spl_token::state::Account, ProgramError> {
    if account.owner != &spl_token::ID {
        msg!("Token account must be owned by SPL Token");
        return Err(ProgramError::IllegalOwner);
    }
    let data = account.try_borrow_data()?;
    spl_token::state::Account::unpack(&data).map_err(|_| ProgramError::InvalidAccountData)
}

fn validate_token_account(
    account: &AccountInfo,
    expected_mint: &Pubkey,
    expected_owner: &Pubkey,
) -> ProgramResult {
    let token = load_token_account(account)?;
    if token.mint != *expected_mint || token.owner != *expected_owner {
        msg!("Token account mint/owner mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
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

/// Update the reward accumulator in MRC.
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

/// Verify CoinConfig PDA and return authority.
fn load_coin_config(
    coin_cfg_account: &AccountInfo,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<CoinConfig, ProgramError> {
    let (expected_cfg, _) = Pubkey::find_program_address(&coin_cfg_seeds(coin_mint), program_id);
    if *coin_cfg_account.key != expected_cfg { return Err(ProgramError::InvalidSeeds); }
    if coin_cfg_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    let cfg_data = coin_cfg_account.try_borrow_data()?;
    CoinConfig::deserialize(&cfg_data)
}

fn validate_governance_authority(
    authority: &AccountInfo,
    coin_mint: &Pubkey,
    rewards_program: &Pubkey,
) -> ProgramResult {
    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if authority.owner != &governance_program_id() {
        msg!("Authority must be the governance adapter PDA");
        return Err(ProgramError::IllegalOwner);
    }

    let (expected, _) = governance_authority_address(rewards_program, coin_mint);
    if *authority.key != expected {
        msg!("Governance authority PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }

    Ok(())
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
        IX_DRAW_INSURANCE => process_draw_insurance(program_id, accounts, &mut data),
        IX_REGISTER_INSURANCE_OPERATOR => process_register_insurance_operator(program_id, accounts),
        IX_PULL_INSURANCE => process_pull_insurance(program_id, accounts, &mut data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ============================================================================
// init_coin_config
// ============================================================================
// Accounts:
//   [0] payer (signer, writable)
//   [1] authority (signer, read-only governance PDA)
//   [2] coin_mint (read-only)
//   [3] coin_config PDA (writable, to create)
//   [4] system_program

fn process_init_coin_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    _data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;

    // Validate coin_mint is a real SPL Token mint
    if coin_mint.owner != &spl_token::ID {
        msg!("COIN mint must be owned by SPL Token program");
        return Err(ProgramError::IllegalOwner);
    }
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
    create_pda_account(payer, coin_cfg_account, system_program, program_id, &seeds, COIN_CFG_SIZE)?;

    let mut cfg_data = coin_cfg_account.try_borrow_mut_data()?;
    let cfg = CoinConfig { authority: *authority.key };
    cfg.serialize(&mut cfg_data);

    Ok(())
}

// ============================================================================
// init_market_rewards
// ============================================================================
// Accounts:
//   [0] payer (signer, writable)
//   [1] authority (signer, read-only governance PDA — must match CoinConfig.authority)
//   [2] market_slab (read-only)
//   [3] mrc PDA (writable, to create)
//   [4] coin_mint (read-only)
//   [5] coin_config PDA (read-only)
//   [6] collateral_mint (read-only)
//   [7] stake_vault PDA (writable, to create — SPL token account)
//   [8] token_program
//   [9] rent sysvar
//   [10] system_program
//
// Data: N (u64), epoch_slots (u64)

fn process_init_market_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
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

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;

    let n_per_epoch = read_u64(data)?;
    let epoch_slots = read_u64(data)?;

    if epoch_slots == 0 {
        msg!("epoch_slots must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    validate_governance_authority(authority, coin_mint.key, program_id)?;

    // Verify CoinConfig PDA and authority
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;

    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Verify slab is initialized and admin is burned.
    let slab_data = market_slab.try_borrow_data()?;
    let header = state::read_header(&slab_data);
    if header.magic == 0 {
        msg!("Slab not initialized (magic = 0)");
        return Err(ProgramError::InvalidAccountData);
    }
    if header.admin != [0u8; 32] {
        msg!("Percolator market admin must be burned before rewards init");
        return Err(ProgramError::InvalidAccountData);
    }
    drop(slab_data);
    // Use current clock slot as the market start for reward tracking
    let clock_for_init = Clock::get()?;
    let market_start_slot = clock_for_init.slot;

    // Create MarketRewardsCfg PDA (init guard)
    let seeds = mrc_seeds(market_slab.key);
    create_pda_account(payer, mrc_account, system_program, program_id, &seeds, MRC_SIZE)?;

    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let cfg = MarketRewardsCfg {
        market_slab: *market_slab.key,
        coin_mint: *coin_mint.key,
        collateral_mint: *collateral_mint.key,
        n_per_epoch,
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
            payer.key,
            stake_vault.key,
            rent.minimum_balance(spl_token::state::Account::LEN),
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        &[payer.clone(), stake_vault.clone(), system_program.clone()],
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
    verify_token_program(token_program)?;
    validate_token_account(user_ata, &cfg.collateral_mint, user.key)?;
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_account.key)?;

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);

    // Load or create StakePosition
    let sp_seeds_arr = sp_seeds(&cfg.market_slab, user.key);
    let (expected_sp, _) = Pubkey::find_program_address(&sp_seeds_arr, program_id);
    if *sp_account.key != expected_sp { return Err(ProgramError::InvalidSeeds); }

    let mut pos = if sp_account.data_len() == 0 || sp_account.lamports() == 0 {
        // First stake (or re-stake after full withdrawal closed the account)
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
    pos.deposit_slot = clock.slot;
    pos.reward_per_token_paid = cfg.reward_per_token_stored;

    // Write position
    let mut sp_data = sp_account.try_borrow_mut_data()?;
    pos.serialize(&mut sp_data);

    Ok(())
}

// ============================================================================
// withdraw — return collateral + claim pending COIN rewards (no lockup)
// ============================================================================
// WITHDRAWAL GUARANTEE: this instruction is fully permissionless. No
// governance action can prevent a depositor from calling unstake.
// draw_insurance can only draw PROFITS (excess above total_staked), so
// depositor capital is always fully backed. The proportional withdrawal
// math is defense-in-depth only. Every account and PDA in this path is
// either user-controlled or program-derived — no governance approval is
// needed, no governance key is checked, and no governance-modifiable
// state gates the transfer.
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
    verify_token_program(token_program)?;
    validate_token_account(user_ata, &cfg.collateral_mint, user.key)?;
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_account.key)?;
    validate_token_account(user_coin_ata, &cfg.coin_mint, user.key)?;

    let clock = Clock::from_account_info(clock_info)?;

    // Update accumulator
    update_accumulator(&mut cfg, clock.slot);

    // Load and verify StakePosition PDA belongs to this user
    let sp_seeds_arr = sp_seeds(&cfg.market_slab, user.key);
    let (expected_sp, _) = Pubkey::find_program_address(&sp_seeds_arr, program_id);
    if *sp_account.key != expected_sp { return Err(ProgramError::InvalidSeeds); }
    if sp_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    let sp_data_r = sp_account.try_borrow_data()?;
    let mut pos = StakePosition::deserialize(&sp_data_r)?;
    drop(sp_data_r);

    if amount > pos.amount {
        msg!("Unstake amount exceeds staked balance");
        return Err(ProgramError::InsufficientFunds);
    }

    // Settle pending rewards
    settle_pending(&mut pos, cfg.reward_per_token_stored);

    // Proportional withdrawal: if insurance draw depleted the vault,
    // everyone takes the same haircut.
    // actual_withdrawal = (amount * vault_balance) / total_staked
    let vault_token = load_token_account(stake_vault)?;
    let vault_balance = vault_token.amount;
    let actual_withdrawal = if vault_balance >= cfg.total_staked {
        // Vault fully backed — no haircut
        amount
    } else if cfg.total_staked == 0 {
        0
    } else {
        // Vault underfunded — proportional haircut
        let w = (amount as u128)
            .checked_mul(vault_balance as u128)
            .ok_or(ProgramError::ArithmeticOverflow)?
            / (cfg.total_staked as u128);
        // Cap at amount (prevent rounding-up exploitation)
        core::cmp::min(w, amount as u128) as u64
    };

    // Update MRC total_staked and serialize before CPI (so re-reads see updated state)
    cfg.total_staked = cfg.total_staked.saturating_sub(amount);
    cfg.serialize(&mut mrc_data);

    // Transfer proportional collateral from vault to user (signed by MRC PDA)
    let mrc_seeds_arr = mrc_seeds(&cfg.market_slab);
    let (_, mrc_bump) = Pubkey::find_program_address(&mrc_seeds_arr, program_id);
    let mrc_signer: [&[u8]; 3] = [b"mrc", cfg.market_slab.as_ref(), &[mrc_bump]];

    if actual_withdrawal > 0 {
        let xfer_ix = spl_token::instruction::transfer(
            token_program.key, stake_vault.key, user_ata.key, mrc_account.key, &[], actual_withdrawal,
        )?;
        drop(mrc_data); // release for CPI
        invoke_signed(
            &xfer_ix,
            &[stake_vault.clone(), user_ata.clone(), mrc_account.clone(), token_program.clone()],
            &[&mrc_signer],
        )?;
    } else {
        drop(mrc_data);
    }

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
    verify_token_program(token_program)?;
    validate_token_account(user_coin_ata, &cfg.coin_mint, user.key)?;

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
// draw_insurance — governance-gated profit withdrawal from vault
// ============================================================================
// The DAO draws PROFITS from the deposit vault — the excess above depositor
// capital (total_staked). Depositor capital is always protected: the DAO
// cannot draw below total_staked. When all depositors have withdrawn
// (total_staked == 0), the DAO can draw whatever remains.
//
// Accounts:
//   [0] payer (signer)
//   [1] authority (signer, governance PDA — must match CoinConfig.authority)
//   [2] mrc PDA (read-only, vault authority for signing)
//   [3] market_slab (read-only)
//   [4] stake_vault (writable)
//   [5] destination (writable — where collateral goes)
//   [6] coin_mint (read-only — for governance authority verification)
//   [7] coin_config PDA (read-only)
//   [8] token_program
//
// Data: amount (u64)

fn process_draw_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 { return Err(ProgramError::InvalidInstructionData); }
    if !payer.is_signer { return Err(ProgramError::MissingRequiredSignature); }

    verify_token_program(token_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;

    // Verify CoinConfig and authority match
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Verify MRC PDA
    let mrc_data = mrc_account.try_borrow_data()?;
    let cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    let (expected_mrc, _) = Pubkey::find_program_address(&mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_account.key != expected_mrc { return Err(ProgramError::InvalidSeeds); }
    if mrc_account.owner != program_id { return Err(ProgramError::IllegalOwner); }
    if *market_slab.key != cfg.market_slab { return Err(ProgramError::InvalidAccountData); }
    if *coin_mint.key != cfg.coin_mint { return Err(ProgramError::InvalidAccountData); }
    drop(mrc_data);

    // Verify stake vault PDA
    let (expected_vault, _) = Pubkey::find_program_address(&stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault { return Err(ProgramError::InvalidSeeds); }
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_account.key)?;

    // Verify destination is correct mint
    let dest_token = load_token_account(destination)?;
    if dest_token.mint != cfg.collateral_mint {
        msg!("Destination mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    // DAO can only draw PROFITS: excess above depositor capital (total_staked).
    // Depositor capital is always protected — the DAO cannot haircut depositors.
    // When total_staked == 0 (all depositors withdrew), DAO can draw everything.
    let vault_token = load_token_account(stake_vault)?;
    let available_profit = vault_token.amount.saturating_sub(cfg.total_staked);
    if amount > available_profit {
        msg!("Draw exceeds available profit (vault_balance - total_staked)");
        return Err(ProgramError::InsufficientFunds);
    }

    // Transfer from vault to destination (signed by MRC PDA)
    let mrc_seeds_arr = mrc_seeds(&cfg.market_slab);
    let (_, mrc_bump) = Pubkey::find_program_address(&mrc_seeds_arr, program_id);
    let mrc_signer: [&[u8]; 3] = [b"mrc", cfg.market_slab.as_ref(), &[mrc_bump]];

    let xfer_ix = spl_token::instruction::transfer(
        token_program.key, stake_vault.key, destination.key, mrc_account.key, &[], amount,
    )?;
    invoke_signed(
        &xfer_ix,
        &[stake_vault.clone(), destination.clone(), mrc_account.clone(), token_program.clone()],
        &[&mrc_signer],
    )
}

// ============================================================================
// register_insurance_operator — set MRC PDA as percolator insurance_operator
// ============================================================================
// Called by the current percolator admin BEFORE admin burn to transfer the
// insurance_operator authority to our MRC PDA. After this, the MRC PDA is
// the only account that can call WithdrawInsuranceLimited on that market,
// which our program uses via pull_insurance to capture profits into the
// stake_vault.
//
// Accounts:
//   [0] admin (signer — current percolator admin)
//   [1] mrc_pda (not a signer here; we sign for it via invoke_signed)
//   [2] market_slab (writable — percolator mutates the header)
//   [3] percolator_program
//
// Data: (none)

fn process_register_insurance_operator<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let admin = next_account_info(iter)?;
    let mrc_pda_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Derive expected MRC PDA from market_slab and verify the passed account matches.
    let seeds_arr = mrc_seeds(market_slab.key);
    let (expected_mrc, mrc_bump) = Pubkey::find_program_address(&seeds_arr, program_id);
    if *mrc_pda_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }

    // Build percolator UpdateAuthority { kind: INSURANCE_OPERATOR, new: MRC_PDA }.
    // Wire format: tag(1) + kind(1) + pubkey(32) = 34 bytes.
    let mut ix_data = alloc::vec::Vec::with_capacity(34);
    ix_data.push(PERC_IX_UPDATE_AUTHORITY);
    ix_data.push(PERC_AUTHORITY_INSURANCE_OPERATOR);
    ix_data.extend_from_slice(expected_mrc.as_ref());

    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*admin.key, true),
            solana_program::instruction::AccountMeta::new_readonly(expected_mrc, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
        ],
        data: ix_data,
    };

    let bump_bytes = [mrc_bump];
    let signer_seeds: [&[u8]; 3] = [b"mrc", market_slab.key.as_ref(), &bump_bytes];
    invoke_signed(
        &ix,
        &[
            admin.clone(),
            mrc_pda_account.clone(),
            market_slab.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// ============================================================================
// pull_insurance — capture profit from percolator insurance into stake_vault
// ============================================================================
// Permissionless keeper instruction. CPIs percolator's WithdrawInsuranceLimited
// with MRC PDA as the insurance_operator (signed via invoke_signed), and
// destination = our stake_vault. The bps cap and cooldown on percolator's
// side gate the rate at which fees can be swept. Once funds land in the
// stake_vault, draw_insurance (DAO-gated, profit-only) is how the DAO
// realizes the profit; user unstake continues to draw from the same vault.
//
// Accounts:
//   [0] payer (signer — anyone, pays CPI fees)
//   [1] mrc PDA (writable is NOT needed; we read only, but percolator wants operator signer)
//   [2] market_slab (writable)
//   [3] operator_ata = stake_vault (writable)
//   [4] percolator_vault (writable — source)
//   [5] token_program
//   [6] percolator_vault_pda (signing vault authority on percolator side)
//   [7] clock
//   [8] percolator_program
//
// Data: amount (u64)

fn process_pull_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let mrc_pda_account = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 { return Err(ProgramError::InvalidInstructionData); }
    if !payer.is_signer { return Err(ProgramError::MissingRequiredSignature); }

    verify_token_program(token_program)?;

    // Verify MRC PDA & derive seeds
    let mrc_data = mrc_account_data_ref(mrc_pda_account, program_id, market_slab.key)?;
    let cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    drop(mrc_data);

    let (expected_mrc, mrc_bump) = Pubkey::find_program_address(
        &mrc_seeds(&cfg.market_slab), program_id);
    if *mrc_pda_account.key != expected_mrc { return Err(ProgramError::InvalidSeeds); }

    // Verify stake_vault PDA (destination for the CPI)
    let (expected_vault, _) = Pubkey::find_program_address(
        &stake_vault_seeds(&cfg.market_slab), program_id);
    if *stake_vault.key != expected_vault { return Err(ProgramError::InvalidSeeds); }
    validate_token_account(stake_vault, &cfg.collateral_mint, mrc_pda_account.key)?;

    // Build WithdrawInsuranceLimited(amount) — tag 23.
    // Accounts: [operator, slab, operator_ata, vault, token_program, vault_pda, clock]
    let mut ix_data = alloc::vec::Vec::with_capacity(9);
    ix_data.push(PERC_IX_WITHDRAW_INSURANCE_LIMITED);
    ix_data.extend_from_slice(&amount.to_le_bytes());

    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(expected_mrc, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new(*stake_vault.key, false),
            solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*percolator_vault_pda.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*clock_info.key, false),
        ],
        data: ix_data,
    };

    let bump_bytes = [mrc_bump];
    let signer_seeds: [&[u8]; 3] = [b"mrc", cfg.market_slab.as_ref(), &bump_bytes];
    invoke_signed(
        &ix,
        &[
            mrc_pda_account.clone(),
            market_slab.clone(),
            stake_vault.clone(),
            percolator_vault.clone(),
            token_program.clone(),
            percolator_vault_pda.clone(),
            clock_info.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )
}

/// Helper: borrow MRC data and verify the account is a valid MRC PDA for the given slab.
fn mrc_account_data_ref<'a>(
    mrc_account: &'a AccountInfo,
    program_id: &Pubkey,
    _market_slab: &Pubkey,
) -> Result<core::cell::Ref<'a, &'a mut [u8]>, ProgramError> {
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let data = mrc_account.try_borrow_data()?;
    // Basic size/disc check — the full PDA check must be done by the caller
    // after reading the slab key from the data.
    if data.len() < MRC_SIZE || data[..8] != MRC_DISC {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(data)
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
