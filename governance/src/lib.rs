#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

#[allow(unused_imports)]
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

pub fn id() -> Pubkey {
    Pubkey::new_from_array([7u8; 32])
}

const IX_INIT_AUTHORITY: u8 = 0;
const IX_INIT_COIN_CONFIG: u8 = 1;
const IX_INIT_MARKET_REWARDS: u8 = 2;
const IX_DRAW_INSURANCE: u8 = 3;

fn read_u8(data: &mut &[u8]) -> Result<u8, ProgramError> {
    if data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let value = data[0];
    *data = &data[1..];
    Ok(value)
}

fn read_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let value = u64::from_le_bytes(data[..8].try_into().unwrap());
    *data = &data[8..];
    Ok(value)
}

fn authority_seeds<'a>(rewards_program: &'a Pubkey, coin_mint: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"rewards_authority", rewards_program.as_ref(), coin_mint.as_ref()]
}

fn authority_signer_seeds<'a>(
    rewards_program: &'a Pubkey,
    coin_mint: &'a Pubkey,
    bump: &'a [u8; 1],
) -> [&'a [u8]; 4] {
    [
        b"rewards_authority",
        rewards_program.as_ref(),
        coin_mint.as_ref(),
        bump,
    ]
}

pub fn authority_address(rewards_program: &Pubkey, coin_mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&authority_seeds(rewards_program, coin_mint), &id())
}

fn verify_authority<'a>(
    program_id: &Pubkey,
    authority: &AccountInfo<'a>,
    rewards_program: &Pubkey,
    coin_mint: &Pubkey,
) -> Result<u8, ProgramError> {
    let (expected, bump) = authority_address(rewards_program, coin_mint);
    if *authority.key != expected {
        msg!("Governance authority PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }
    if authority.owner != program_id {
        msg!("Governance authority must be owned by governance adapter");
        return Err(ProgramError::IllegalOwner);
    }
    Ok(bump)
}

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub fn process_instruction<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    instruction_data: &[u8],
) -> ProgramResult {
    let mut data = instruction_data;
    match read_u8(&mut data)? {
        IX_INIT_AUTHORITY => process_init_authority(program_id, accounts),
        IX_INIT_COIN_CONFIG => process_init_coin_config(program_id, accounts),
        IX_INIT_MARKET_REWARDS => process_init_market_rewards(program_id, accounts, &mut data),
        IX_DRAW_INSURANCE => process_draw_insurance(program_id, accounts, &mut data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn process_init_authority<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected, bump) = authority_address(rewards_program.key, coin_mint.key);
    if *authority.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }

    if authority.lamports() > 0 {
        if authority.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        return Ok(());
    }

    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let rent = Rent::get()?;
    invoke_signed(
        &system_instruction::create_account(
            payer.key,
            authority.key,
            rent.minimum_balance(0).max(1),
            0,
            program_id,
        ),
        &[payer.clone(), authority.clone(), system_program.clone()],
        &[&signer_seeds],
    )
}

fn process_init_coin_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let bump = verify_authority(program_id, authority, rewards_program.key, coin_mint.key)?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new(*coin_cfg.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
        ],
        data: vec![3u8],
    };

    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            system_program.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_init_market_rewards<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let mrc = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let n_per_epoch = read_u64(data)?;
    let epoch_slots = read_u64(data)?;
    let bump = verify_authority(program_id, authority, rewards_program.key, coin_mint.key)?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);

    let mut ix_data = Vec::with_capacity(17);
    ix_data.push(0u8);
    ix_data.extend_from_slice(&n_per_epoch.to_le_bytes());
    ix_data.extend_from_slice(&epoch_slots.to_le_bytes());
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*market_slab.key, false),
            AccountMeta::new(*mrc.key, false),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new_readonly(*collateral_mint.key, false),
            AccountMeta::new(*stake_vault.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
            AccountMeta::new_readonly(*rent_sysvar.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
        ],
        data: ix_data,
    };

    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            market_slab.clone(),
            mrc.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            collateral_mint.clone(),
            stake_vault.clone(),
            token_program.clone(),
            rent_sysvar.clone(),
            system_program.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_draw_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let mrc = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let stake_vault = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let amount = read_u64(data)?;
    let bump = verify_authority(program_id, authority, rewards_program.key, coin_mint.key)?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);

    let mut ix_data = Vec::with_capacity(9);
    ix_data.push(5u8); // IX_DRAW_INSURANCE
    ix_data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*mrc.key, false),
            AccountMeta::new_readonly(*market_slab.key, false),
            AccountMeta::new(*stake_vault.key, false),
            AccountMeta::new(*destination.key, false),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: ix_data,
    };

    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            mrc.clone(),
            market_slab.clone(),
            stake_vault.clone(),
            destination.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            token_program.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

