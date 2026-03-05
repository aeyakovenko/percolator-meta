//! Rewards program: COIN mint-authority PDA, owner-reward claims, LP-reward claims.
//! Non-upgradeable. No admin keys. ~200 lines of logic.

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

/// Slots per epoch — must match the value committed in the proposal hash.
pub const EPOCH_SLOTS: u64 = 216_000; // ~1 day at 2.5 slots/sec

/// Fixed-point scale for LP reward math.
pub const FP: u128 = 1u128 << 64;

/// Hard cap on K to bound COIN inflation from LP rewards.
pub const MAX_LP_COIN_PER_FEE_FP: u128 = 1_000_000u128 << 64; // 1M COIN per fee-atom max

/// Instruction tags
const IX_INIT_MARKET_REWARDS: u8 = 0;
const IX_CLAIM_OWNER_REWARDS: u8 = 1;
const IX_CLAIM_LP_REWARDS: u8 = 2;
const IX_INIT_COIN_CONFIG: u8 = 3;

// ============================================================================
// Account sizes
// ============================================================================

/// MarketRewardsCfg: 8 (discriminator) + 32 + 32 + 32 + 8 + 16 + 8 + 8 = 144
const MRC_SIZE: usize = 8 + 32 + 32 + 32 + 8 + 16 + 8 + 8;
/// OwnerClaimState: 8 (discriminator) + 8 = 16
const OCS_SIZE: usize = 8 + 8;
/// LpClaimState: 8 (discriminator) + 32 = 40 (u256 = 32 bytes)
const LCS_SIZE: usize = 8 + 32;
/// CoinConfig: 8 (discriminator) + 32 (authority) + 32 (receipt_program) = 72
const COIN_CFG_SIZE: usize = 8 + 32 + 32;

// Discriminators (first 8 bytes)
const MRC_DISC: [u8; 8] = *b"MRC_INIT";
const OCS_DISC: [u8; 8] = *b"OCS_INIT";
const LCS_DISC: [u8; 8] = *b"LCS_INIT";
const COIN_CFG_DISC: [u8; 8] = *b"CCFG_INI";

// ============================================================================
// PDA seeds
// ============================================================================

fn mrc_seeds(market_slab: &Pubkey) -> [&[u8]; 2] {
    [b"mrc", market_slab.as_ref()]
}

fn ocs_seeds<'a>(market_slab: &'a Pubkey, receipt: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"ocs", market_slab.as_ref(), receipt.as_ref()]
}

fn mint_authority_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_mint_authority", coin_mint.as_ref()]
}

fn coin_cfg_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_cfg", coin_mint.as_ref()]
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
// MarketRewardsCfg read/write
// ============================================================================

struct MarketRewardsCfg {
    market_slab: Pubkey,
    coin_mint: Pubkey,
    receipt_program: Pubkey,
    n: u64,
    k: u128,
    market_start_slot: u64,
    total_contributed_lamports: u64,
}

impl MarketRewardsCfg {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < MRC_SIZE { return Err(ProgramError::InvalidAccountData); }
        if data[..8] != MRC_DISC { return Err(ProgramError::InvalidAccountData); }
        let mut off = 8;
        let market_slab = Pubkey::new_from_array(data[off..off+32].try_into().unwrap()); off += 32;
        let coin_mint = Pubkey::new_from_array(data[off..off+32].try_into().unwrap()); off += 32;
        let receipt_program = Pubkey::new_from_array(data[off..off+32].try_into().unwrap()); off += 32;
        let n = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let k = u128::from_le_bytes(data[off..off+16].try_into().unwrap()); off += 16;
        let market_start_slot = u64::from_le_bytes(data[off..off+8].try_into().unwrap()); off += 8;
        let total_contributed_lamports = u64::from_le_bytes(data[off..off+8].try_into().unwrap());
        Ok(Self { market_slab, coin_mint, receipt_program, n, k, market_start_slot, total_contributed_lamports })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&MRC_DISC);
        let mut off = 8;
        data[off..off+32].copy_from_slice(self.market_slab.as_ref()); off += 32;
        data[off..off+32].copy_from_slice(self.coin_mint.as_ref()); off += 32;
        data[off..off+32].copy_from_slice(self.receipt_program.as_ref()); off += 32;
        data[off..off+8].copy_from_slice(&self.n.to_le_bytes()); off += 8;
        data[off..off+16].copy_from_slice(&self.k.to_le_bytes()); off += 16;
        data[off..off+8].copy_from_slice(&self.market_start_slot.to_le_bytes()); off += 8;
        data[off..off+8].copy_from_slice(&self.total_contributed_lamports.to_le_bytes());
    }
}

// ============================================================================
// CoinConfig — shared across all markets using the same COIN mint
// ============================================================================

struct CoinConfig {
    authority: Pubkey,
    receipt_program: Pubkey,
}

impl CoinConfig {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < COIN_CFG_SIZE { return Err(ProgramError::InvalidAccountData); }
        if data[..8] != COIN_CFG_DISC { return Err(ProgramError::InvalidAccountData); }
        let authority = Pubkey::new_from_array(data[8..40].try_into().unwrap());
        let receipt_program = Pubkey::new_from_array(data[40..72].try_into().unwrap());
        Ok(Self { authority, receipt_program })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&COIN_CFG_DISC);
        data[8..40].copy_from_slice(self.authority.as_ref());
        data[40..72].copy_from_slice(self.receipt_program.as_ref());
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

/// Read LP owner pubkey from slab account data (Account.owner field).
fn read_lp_owner_from_slab(slab_data: &[u8], lp_idx: u16) -> Result<Pubkey, ProgramError> {
    let engine_data = &slab_data[ENGINE_OFF..];
    // We need to find the Account struct for lp_idx inside the RiskEngine.
    // The Account struct offset within the engine depends on the engine layout.
    // RiskEngine.accounts is at a fixed offset, and each Account has a fixed size.
    let acct = read_account_from_engine(engine_data, lp_idx)?;
    // Verify it's an LP (kind == 1)
    if acct.kind != 1 {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(Pubkey::new_from_array(acct.owner))
}

/// Minimal account data we need from the engine.
struct AccountSlice {
    kind: u8,
    owner: [u8; 32],
    fees_earned_total: u128,
}

/// Read specific account fields from engine data.
/// This is layout-dependent and must match the RiskEngine/Account layout in percolator.
fn read_account_from_engine(engine_data: &[u8], idx: u16) -> Result<AccountSlice, ProgramError> {
    // We need to compute the offset of accounts[idx] within RiskEngine.
    // Rather than hardcode offsets (fragile), we use percolator's size_of::<Account>()
    // and the known offset of the accounts array.
    //
    // For a safer approach, we read through percolator-prog's zc module.
    // But since we're in a different program, we need to parse the raw bytes.
    //
    // Account layout (repr(C), all fields known):
    // account_id: u64 (8)
    // capital: U128 = [u64;2] (16)
    // kind: u8 (1)
    // pnl: I128 = [u64;2] (16)
    // reserved_pnl: u64 (8)
    // warmup_started_at_slot: u64 (8)
    // warmup_slope_per_step: U128 (16)
    // position_size: I128 (16)
    // entry_price: u64 (8)
    // funding_index: I128 (16)
    // matcher_program: [u8;32] (32)
    // matcher_context: [u8;32] (32)
    // owner: [u8;32] (32)
    // fee_credits: I128 (16)
    // last_fee_slot: u64 (8)
    // fees_earned_total: U128 (16)
    //
    // Total Account size = 8+16+1+16+8+8+16+16+8+16+32+32+32+16+8+16 = 249
    // With repr(C) and u64 alignment for [u64;2], padding after kind(u8):
    // After kind(u8), next field pnl requires alignment of 8 (u64), so 7 bytes padding
    // Total = 8+16+1+7+16+8+8+16+16+8+16+32+32+32+16+8+16 = 256

    use core::mem::size_of;
    let account_size: usize = size_of::<percolator::Account>();

    // RiskEngine fields before accounts array:
    // We need the offset of the `accounts` field.
    // Rather than computing manually, use offset calculation.
    // The RiskEngine has:
    //   vault: U128(16), insurance_fund: InsuranceFund(32), params: RiskParams(...),
    //   ... many fields, then used[BITMAP_WORDS], num_used_accounts, next_account_id,
    //   free_head, next_free[MAX_ACCOUNTS], accounts[MAX_ACCOUNTS]
    //
    // For correctness, we compute accounts_offset from RiskEngine's size and known tail.
    let engine_size = size_of::<percolator::RiskEngine>();
    let max_accounts = percolator::MAX_ACCOUNTS;
    // accounts is the LAST field: engine_size = accounts_offset + max_accounts * account_size
    let accounts_offset = engine_size - max_accounts * account_size;

    let acct_start = accounts_offset + (idx as usize) * account_size;
    let acct_end = acct_start + account_size;
    if acct_end > engine_data.len() {
        return Err(ProgramError::InvalidAccountData);
    }

    let acct_data = &engine_data[acct_start..acct_end];

    // kind is at offset 24 (after account_id:8 + capital:16)
    let kind = acct_data[24];

    // owner is at known offset within Account.
    // account_id(8) + capital(16) + kind(1) + pad(7) + pnl(16) + reserved_pnl(8) +
    // warmup_started_at_slot(8) + warmup_slope_per_step(16) + position_size(16) +
    // entry_price(8) + funding_index(16) + matcher_program(32) + matcher_context(32) = 184
    let owner_off = 8 + 16 + 1 + 7 + 16 + 8 + 8 + 16 + 16 + 8 + 16 + 32 + 32;
    let owner: [u8; 32] = acct_data[owner_off..owner_off+32].try_into().unwrap();

    // fees_earned_total is after owner(32) + fee_credits(16) + last_fee_slot(8) = 56 bytes after owner_off
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
        IX_CLAIM_OWNER_REWARDS => process_claim_owner_rewards(program_id, accounts, &mut data),
        IX_CLAIM_LP_REWARDS => process_claim_lp_rewards(program_id, accounts, &mut data),
        IX_INIT_COIN_CONFIG => process_init_coin_config(program_id, accounts, &mut data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// ============================================================================
// init_coin_config
// ============================================================================
// Accounts:
//   [0] authority (signer, writable — pays for PDA creation)
//   [1] coin_mint (read-only)
//   [2] coin_config PDA (writable, to create)
//   [3] receipt_program (read-only) — MetaDAO program that owns receipt accounts
//   [4] system_program

fn process_init_coin_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    _data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let receipt_program = next_account_info(iter)?;
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

    // Create CoinConfig PDA (init guard — fails if already exists)
    let seeds = coin_cfg_seeds(coin_mint.key);
    create_pda_account(authority, coin_cfg_account, system_program, program_id, &seeds, COIN_CFG_SIZE)?;

    // Write config
    let mut cfg_data = coin_cfg_account.try_borrow_mut_data()?;
    let cfg = CoinConfig {
        authority: *authority.key,
        receipt_program: *receipt_program.key,
    };
    cfg.serialize(&mut cfg_data);

    Ok(())
}

// ============================================================================
// init_market_rewards
// ============================================================================
// Accounts:
//   [0] authority (signer, writable — must match CoinConfig.authority)
//   [1] market_slab (read-only)
//   [2] market_rewards_cfg PDA (writable, to create)
//   [3] coin_mint (read-only)
//   [4] coin_config PDA (read-only)
//   [5] system_program
//
// Data: N (u64), K (u128), total_contributed_lamports (u64)

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
    let system_program = next_account_info(iter)?;

    if !authority.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let n = read_u64(data)?;
    let k = read_u128(data)?;
    let total_contributed_lamports = read_u64(data)?;

    // Validate K is within bounds
    if k > MAX_LP_COIN_PER_FEE_FP {
        msg!("K exceeds MAX_LP_COIN_PER_FEE_FP");
        return Err(ProgramError::InvalidInstructionData);
    }

    // Verify CoinConfig PDA
    let (expected_cfg, _) = Pubkey::find_program_address(
        &coin_cfg_seeds(coin_mint.key),
        program_id,
    );
    if *coin_cfg_account.key != expected_cfg {
        return Err(ProgramError::InvalidSeeds);
    }
    if coin_cfg_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }

    // Read CoinConfig and verify authority
    let cfg_data = coin_cfg_account.try_borrow_data()?;
    let coin_cfg = CoinConfig::deserialize(&cfg_data)?;
    drop(cfg_data);

    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Read market_start_slot from slab (§2.1) — never trust caller-supplied value
    let slab_data = market_slab.try_borrow_data()?;
    let market_start_slot = state::read_market_start_slot(&slab_data);
    if market_start_slot == 0 {
        msg!("market_start_slot is 0; slab not initialized via futarchy flow");
        return Err(ProgramError::InvalidAccountData);
    }
    drop(slab_data);

    // Create MarketRewardsCfg PDA (init guard — fails if already exists)
    let seeds = mrc_seeds(market_slab.key);
    create_pda_account(authority, mrc_account, system_program, program_id, &seeds, MRC_SIZE)?;

    // Write config — receipt_program is copied from CoinConfig
    let mut mrc_data = mrc_account.try_borrow_mut_data()?;
    let cfg = MarketRewardsCfg {
        market_slab: *market_slab.key,
        coin_mint: *coin_mint.key,
        receipt_program: coin_cfg.receipt_program,
        n,
        k,
        market_start_slot,
        total_contributed_lamports,
    };
    cfg.serialize(&mut mrc_data);

    Ok(())
}

// ============================================================================
// claim_owner_rewards
// ============================================================================
// Accounts:
//   [0] contributor (signer)
//   [1] market_rewards_cfg PDA (read-only)
//   [2] market_slab (read-only) — unused now but kept for future extensibility
//   [3] receipt (read-only) — MetaDAO receipt with contributed_lamports
//   [4] owner_claim_state PDA (writable, created on first claim)
//   [5] coin_mint (writable)
//   [6] coin_ata (writable) — contributor's COIN token account
//   [7] mint_authority PDA (read-only)
//   [8] token_program
//   [9] system_program
//   [10] clock sysvar
//
// Data: (none — all derived from accounts)
//
// Receipt layout (MetaDAO): we read contributor pubkey and contributed_lamports.
// For now we define a minimal expected layout:
//   offset 0: contributor pubkey (32 bytes)
//   offset 32: contributed_lamports (u64, 8 bytes)

fn process_claim_owner_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    _data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let contributor = next_account_info(iter)?;
    let mrc_account = next_account_info(iter)?;
    let _market_slab = next_account_info(iter)?;
    let receipt = next_account_info(iter)?;
    let ocs_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_ata = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    if !contributor.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Read MarketRewardsCfg
    let mrc_data = mrc_account.try_borrow_data()?;
    let cfg = MarketRewardsCfg::deserialize(&mrc_data)?;
    drop(mrc_data);

    // Verify MRC PDA
    let (expected_mrc, _) = Pubkey::find_program_address(
        &mrc_seeds(&cfg.market_slab),
        program_id,
    );
    if *mrc_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }

    // Verify receipt is owned by the MetaDAO receipt program stored in MRC
    if *receipt.owner != cfg.receipt_program {
        msg!("Receipt account not owned by the expected receipt program");
        return Err(ProgramError::IllegalOwner);
    }

    // Read receipt: contributor pubkey (32) + contributed_lamports (u64)
    let receipt_data = receipt.try_borrow_data()?;
    if receipt_data.len() < 40 {
        return Err(ProgramError::InvalidAccountData);
    }
    let receipt_contributor = Pubkey::new_from_array(receipt_data[..32].try_into().unwrap());
    let contributed_lamports = u64::from_le_bytes(receipt_data[32..40].try_into().unwrap());
    drop(receipt_data);

    // Verify signer is the receipt's contributor
    if *contributor.key != receipt_contributor {
        msg!("Signer does not match receipt contributor");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // Get current slot
    let clock = Clock::from_account_info(clock_info)?;
    let current_slot = clock.slot;

    // Compute epochs elapsed
    let current_epoch = current_slot / EPOCH_SLOTS;
    let start_epoch = cfg.market_start_slot / EPOCH_SLOTS;
    let epochs_elapsed = current_epoch.saturating_sub(start_epoch);

    // total_entitled = N * epochs_elapsed * contributed_lamports / total_contributed_lamports
    // Use u128 for intermediate math to avoid overflow
    let total_entitled: u64 = if cfg.total_contributed_lamports == 0 || epochs_elapsed == 0 {
        0
    } else {
        let num = (cfg.n as u128)
            .checked_mul(epochs_elapsed as u128)
            .and_then(|v| v.checked_mul(contributed_lamports as u128))
            .ok_or(ProgramError::ArithmeticOverflow)?;
        let result = num / (cfg.total_contributed_lamports as u128);
        // Clamp to u64
        if result > u64::MAX as u128 {
            return Err(ProgramError::ArithmeticOverflow);
        }
        result as u64
    };

    // Create or load OwnerClaimState PDA
    let ocs_seeds_arr = ocs_seeds(&cfg.market_slab, receipt.key);
    let (expected_ocs, _) = Pubkey::find_program_address(&ocs_seeds_arr, program_id);
    if *ocs_account.key != expected_ocs {
        return Err(ProgramError::InvalidSeeds);
    }

    let coin_claimed: u64;
    if ocs_account.data_len() == 0 {
        // First claim — create the PDA
        create_pda_account(contributor, ocs_account, system_program, program_id, &ocs_seeds_arr, OCS_SIZE)?;
        let mut ocs_data = ocs_account.try_borrow_mut_data()?;
        ocs_data[..8].copy_from_slice(&OCS_DISC);
        ocs_data[8..16].copy_from_slice(&0u64.to_le_bytes());
        drop(ocs_data);
        coin_claimed = 0;
    } else {
        // Load existing
        if ocs_account.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let ocs_data = ocs_account.try_borrow_data()?;
        if ocs_data.len() < OCS_SIZE || ocs_data[..8] != OCS_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        coin_claimed = u64::from_le_bytes(ocs_data[8..16].try_into().unwrap());
        drop(ocs_data);
    }

    let claimable = total_entitled.saturating_sub(coin_claimed);
    if claimable == 0 {
        return Ok(());
    }

    // Verify coin_mint matches config
    if *coin_mint.key != cfg.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }

    // Verify mint_authority PDA
    let ma_seeds = mint_authority_seeds(&cfg.coin_mint);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }

    // Mint COIN
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", cfg.coin_mint.as_ref(), &bump_bytes];
    mint_coin(token_program, coin_mint, coin_ata, mint_authority, claimable, &signer_seeds)?;

    // Update claim state
    let new_claimed = coin_claimed.checked_add(claimable).ok_or(ProgramError::ArithmeticOverflow)?;
    let mut ocs_data = ocs_account.try_borrow_mut_data()?;
    ocs_data[8..16].copy_from_slice(&new_claimed.to_le_bytes());

    Ok(())
}

// ============================================================================
// claim_lp_rewards
// ============================================================================
// Accounts:
//   [0] lp_owner (signer)
//   [1] market_rewards_cfg PDA (read-only)
//   [2] market_slab (read-only)
//   [3] lp_claim_state PDA (writable, created on first claim)
//   [4] coin_mint (writable)
//   [5] coin_ata (writable)
//   [6] mint_authority PDA (read-only)
//   [7] token_program
//   [8] percolator_program (for CPI)
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
    if *mrc_account.key != expected_mrc {
        return Err(ProgramError::InvalidSeeds);
    }
    if mrc_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }

    // Verify market_slab matches config
    if *market_slab.key != cfg.market_slab {
        return Err(ProgramError::InvalidAccountData);
    }
    // Verify market_slab is owned by the percolator program we CPI into
    if market_slab.owner != percolator_program.key {
        msg!("market_slab owner does not match percolator_program");
        return Err(ProgramError::IllegalOwner);
    }

    // Verify signer is the LP position's owner (read from slab)
    let slab_data = market_slab.try_borrow_data()?;
    let lp_owner_pubkey = read_lp_owner_from_slab(&slab_data, lp_idx)?;
    drop(slab_data);

    if *lp_owner.key != lp_owner_pubkey {
        msg!("Signer does not match LP position owner");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // CPI to percolator::QueryLpFees to get fees_earned_total
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

    // Read return data
    let (returning_program, return_data) = solana_program::program::get_return_data()
        .ok_or(ProgramError::InvalidAccountData)?;
    if returning_program != *percolator_program.key {
        return Err(ProgramError::InvalidAccountData);
    }
    if return_data.len() < 16 {
        return Err(ProgramError::InvalidAccountData);
    }
    let fees_earned_total = u128::from_le_bytes(return_data[..16].try_into().unwrap());

    // LP reward math (§8.3):
    // entitled_fp = fees_earned_total * K  (u256 math)
    // claimable_fp = entitled_fp - reward_claimed_fp
    // claimable_coins = claimable_fp / FP
    // reward_claimed_fp += claimable_coins * FP

    // u256 math using two u128 halves
    // entitled_fp = fees_earned_total * K
    // Both are u128, product can be u256.
    let (entitled_lo, entitled_hi) = mul_u128_wide(fees_earned_total, cfg.k);

    // LpClaimState PDA seeds: [b"lcs", market_slab_key, lp_idx_le_bytes]
    let lp_idx_bytes = lp_idx.to_le_bytes();
    let lcs_seeds_arr: [&[u8]; 3] = [b"lcs", cfg.market_slab.as_ref(), &lp_idx_bytes];
    let (expected_lcs, _) = Pubkey::find_program_address(&lcs_seeds_arr, program_id);
    if *lcs_account.key != expected_lcs {
        return Err(ProgramError::InvalidSeeds);
    }

    let (claimed_lo, claimed_hi): (u128, u128);
    if lcs_account.data_len() == 0 {
        // First claim — create PDA
        create_pda_account(lp_owner, lcs_account, system_program, program_id, &lcs_seeds_arr, LCS_SIZE)?;
        let mut lcs_data = lcs_account.try_borrow_mut_data()?;
        lcs_data[..8].copy_from_slice(&LCS_DISC);
        lcs_data[8..40].copy_from_slice(&[0u8; 32]);
        drop(lcs_data);
        claimed_lo = 0;
        claimed_hi = 0;
    } else {
        if lcs_account.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let lcs_data = lcs_account.try_borrow_data()?;
        if lcs_data.len() < LCS_SIZE || lcs_data[..8] != LCS_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        claimed_lo = u128::from_le_bytes(lcs_data[8..24].try_into().unwrap());
        claimed_hi = u128::from_le_bytes(lcs_data[24..40].try_into().unwrap());
        drop(lcs_data);
    }

    // claimable_fp = entitled_fp - claimed_fp (u256 subtraction)
    let (claimable_lo, claimable_hi) = sub_u256(entitled_lo, entitled_hi, claimed_lo, claimed_hi);

    // claimable_coins = claimable_fp / FP = claimable_fp >> 64
    // Since FP = 2^64, dividing by FP means taking the high 64 bits of claimable_lo
    // plus the full claimable_hi shifted.
    let claimable_coins_u128 = (claimable_lo >> 64) | (claimable_hi << 64);

    // Clamp to u64 for minting
    let claimable_coins: u64 = if claimable_coins_u128 > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    } else {
        claimable_coins_u128 as u64
    };

    if claimable_coins == 0 {
        return Ok(());
    }

    // Verify coin_mint matches config
    if *coin_mint.key != cfg.coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }

    // Mint COIN
    let ma_seeds = mint_authority_seeds(&cfg.coin_mint);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", cfg.coin_mint.as_ref(), &bump_bytes];
    mint_coin(token_program, coin_mint, coin_ata, mint_authority, claimable_coins, &signer_seeds)?;

    // Update LpClaimState: reward_claimed_fp += claimable_coins * FP
    let added_lo = (claimable_coins as u128) << 64;  // claimable_coins * FP
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

/// Multiply two u128 values and return (lo, hi) of the u256 result.
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

/// Subtract two u256 values: (a_lo, a_hi) - (b_lo, b_hi). Saturates to 0.
fn sub_u256(a_lo: u128, a_hi: u128, b_lo: u128, b_hi: u128) -> (u128, u128) {
    if a_hi < b_hi || (a_hi == b_hi && a_lo < b_lo) {
        return (0, 0);
    }
    let (lo, borrow) = a_lo.overflowing_sub(b_lo);
    let hi = a_hi - b_hi - if borrow { 1 } else { 0 };
    (lo, hi)
}

/// Add two u256 values: (a_lo, a_hi) + (b_lo, b_hi). Saturates on overflow.
fn add_u256(a_lo: u128, a_hi: u128, b_lo: u128, b_hi: u128) -> (u128, u128) {
    let (lo, carry) = a_lo.overflowing_add(b_lo);
    let hi = a_hi.saturating_add(b_hi).saturating_add(if carry { 1 } else { 0 });
    (lo, hi)
}
