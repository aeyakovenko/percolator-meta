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
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
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
const IX_MINT_REWARD: u8 = 4;
const IX_TRANSFER_MINT_AUTHORITY: u8 = 6;
const IX_ACTIVATE_LIVE: u8 = 7;
const IX_PERCOLATOR_ADMIN: u8 = 9;
const IX_INIT_GENESIS_BOOTSTRAP: u8 = 10;
// tag 11 (genesis distribution mint) retired: triggering is now permissionless
// on the rewards program — no governance wrapper.
const IX_FINALIZE_GENESIS: u8 = 12;
const IX_DRAW_GENESIS_SURPLUS: u8 = 13;
const IX_KICKSTART_GENESIS_MARKET: u8 = 14;
const IX_RECOVER_GENESIS_MARKET: u8 = 15;
const IX_APPROVE_BUILDER: u8 = 16;
const IX_INIT_GENESIS_SQUADS: u8 = 17;
const IX_HANDOVER_GENESIS_SQUADS: u8 = 18;

// Rewards-program instruction tags this adapter forwards.
const REWARDS_IX_INIT_GENESIS_SQUADS: u8 = 32;
const REWARDS_IX_HANDOVER_GENESIS_SQUADS: u8 = 33;

const AUTHORITY_DISC: [u8; 8] = *b"GAUTH001";
const AUTHORITY_SIZE: usize = 8 + 32;

struct AuthorityConfig {
    controller: Pubkey,
}

impl AuthorityConfig {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < AUTHORITY_SIZE || data[..8] != AUTHORITY_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            controller: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&AUTHORITY_DISC);
        data[8..40].copy_from_slice(self.controller.as_ref());
    }
}

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

fn read_bytes32(data: &mut &[u8]) -> Result<[u8; 32], ProgramError> {
    if data.len() < 32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let value = data[..32].try_into().unwrap();
    *data = &data[32..];
    Ok(value)
}

fn authority_seeds<'a>(rewards_program: &'a Pubkey, coin_mint: &'a Pubkey) -> [&'a [u8]; 3] {
    [
        b"rewards_authority",
        rewards_program.as_ref(),
        coin_mint.as_ref(),
    ]
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
    let data = authority.try_borrow_data()?;
    AuthorityConfig::deserialize(&data)?;
    Ok(bump)
}

fn verify_authority_controller<'a>(
    program_id: &Pubkey,
    payer: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    rewards_program: &Pubkey,
    coin_mint: &Pubkey,
) -> Result<u8, ProgramError> {
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let bump = verify_authority(program_id, authority, rewards_program, coin_mint)?;
    let data = authority.try_borrow_data()?;
    let cfg = AuthorityConfig::deserialize(&data)?;
    if cfg.controller != *payer.key {
        msg!("Governance adapter controller mismatch");
        return Err(ProgramError::MissingRequiredSignature);
    }
    Ok(bump)
}

fn existing_authority_initialized<'a>(
    program_id: &Pubkey,
    authority: &AccountInfo<'a>,
    payer_key: &Pubkey,
) -> Result<bool, ProgramError> {
    if authority.lamports() == 0 && authority.data_len() == 0 {
        return Ok(false);
    }
    if authority.owner == program_id {
        let data = authority.try_borrow_data()?;
        let cfg = AuthorityConfig::deserialize(&data)?;
        if cfg.controller != *payer_key {
            msg!("Governance adapter controller mismatch");
            return Err(ProgramError::MissingRequiredSignature);
        }
        return Ok(true);
    }
    if authority.owner == &solana_program::system_program::ID && authority.data_len() == 0 {
        return Ok(false);
    }
    Err(ProgramError::IllegalOwner)
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
        IX_INIT_COIN_CONFIG => process_init_coin_config(program_id, accounts, &mut data),
        IX_MINT_REWARD => process_mint_reward(program_id, accounts, &mut data),
        IX_TRANSFER_MINT_AUTHORITY => process_transfer_mint_authority(program_id, accounts),
        IX_ACTIVATE_LIVE => process_activate_live(program_id, accounts, &mut data),
        IX_PERCOLATOR_ADMIN => process_percolator_admin(program_id, accounts, &data),
        IX_INIT_GENESIS_BOOTSTRAP => {
            process_init_genesis_bootstrap(program_id, accounts, &mut data)
        }
        IX_FINALIZE_GENESIS => process_finalize_genesis(program_id, accounts, &mut data),
        IX_DRAW_GENESIS_SURPLUS => process_draw_genesis_surplus(program_id, accounts, &mut data),
        IX_KICKSTART_GENESIS_MARKET => {
            process_kickstart_genesis_market(program_id, accounts, &mut data)
        }
        IX_RECOVER_GENESIS_MARKET => {
            process_recover_genesis_market(program_id, accounts, &mut data)
        }
        IX_APPROVE_BUILDER => process_approve_builder(program_id, accounts, &mut data),
        IX_INIT_GENESIS_SQUADS => process_init_genesis_squads(program_id, accounts, &mut data),
        IX_HANDOVER_GENESIS_SQUADS => {
            process_handover_genesis_squads(program_id, accounts, &mut data)
        }
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

    if existing_authority_initialized(program_id, authority, payer.key)? {
        return Ok(());
    }

    if coin_mint.owner != &spl_token::ID {
        msg!("COIN mint must be owned by SPL Token");
        return Err(ProgramError::IllegalOwner);
    }

    let mint_data = coin_mint.try_borrow_data()?;
    let mint = spl_token::state::Mint::unpack(&mint_data)?;
    match mint.mint_authority {
        solana_program::program_option::COption::Some(auth) if auth == *payer.key => {}
        _ => {
            msg!("init_authority signer must be current COIN mint authority");
            return Err(ProgramError::MissingRequiredSignature);
        }
    }
    drop(mint_data);

    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let rent = Rent::get()?;
    let required = rent.minimum_balance(AUTHORITY_SIZE);
    let current = authority.lamports();
    if current < required {
        invoke(
            &system_instruction::transfer(payer.key, authority.key, required - current),
            &[payer.clone(), authority.clone(), system_program.clone()],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(authority.key, AUTHORITY_SIZE as u64),
        &[authority.clone(), system_program.clone()],
        &[&signer_seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(authority.key, program_id),
        &[authority.clone(), system_program.clone()],
        &[&signer_seeds],
    )?;

    let mut data = authority.try_borrow_mut_data()?;
    AuthorityConfig {
        controller: *payer.key,
    }
    .serialize(&mut data);
    Ok(())
}

fn process_init_coin_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let bootstrap_delay_slots = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let mut ix_data = Vec::with_capacity(9);
    ix_data.push(3u8);
    ix_data.extend_from_slice(&bootstrap_delay_slots.to_le_bytes());

    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new(*coin_cfg.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
        ],
        data: ix_data,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dusted_system_authority_pda_remains_initializable() {
        let program_id = id();
        let payer = Pubkey::new_unique();
        let authority_key = Pubkey::new_unique();
        let system_id = solana_program::system_program::ID;
        let mut lamports = 1;
        let mut data = Vec::new();
        let authority = AccountInfo::new(
            &authority_key,
            false,
            true,
            &mut lamports,
            data.as_mut_slice(),
            &system_id,
            false,
            0,
        );

        assert_eq!(
            existing_authority_initialized(&program_id, &authority, &payer),
            Ok(false)
        );
    }
}

fn process_activate_live<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let clock = next_account_info(iter)?;

    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);

    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new(*coin_cfg.key, false),
            AccountMeta::new_readonly(*clock.key, false),
        ],
        data: vec![11u8],
    };

    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            clock.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_percolator_admin<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    percolator_ix_data: &[u8],
) -> ProgramResult {
    if percolator_ix_data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);

    let tail: Vec<AccountInfo<'a>> = iter.cloned().collect();
    let mut ix_accounts = Vec::with_capacity(7 + tail.len());
    ix_accounts.push(AccountMeta::new(*payer.key, true));
    ix_accounts.push(AccountMeta::new_readonly(*authority.key, true));
    ix_accounts.push(AccountMeta::new_readonly(*coin_mint.key, false));
    ix_accounts.push(AccountMeta::new_readonly(*coin_cfg.key, false));
    ix_accounts.push(AccountMeta::new_readonly(*genesis_cfg.key, false));
    ix_accounts.push(if market_admin.is_writable {
        AccountMeta::new(*market_admin.key, false)
    } else {
        AccountMeta::new_readonly(*market_admin.key, false)
    });
    ix_accounts.push(AccountMeta::new_readonly(*percolator_program.key, false));
    for account in tail.iter() {
        if account.is_writable {
            ix_accounts.push(AccountMeta::new(*account.key, account.is_signer));
        } else {
            ix_accounts.push(AccountMeta::new_readonly(*account.key, account.is_signer));
        }
    }

    let mut ix_data = Vec::with_capacity(1 + percolator_ix_data.len());
    ix_data.push(20u8);
    ix_data.extend_from_slice(percolator_ix_data);
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: ix_accounts,
        data: ix_data,
    };

    let mut cpi_accounts = Vec::with_capacity(9 + tail.len());
    cpi_accounts.push(payer.clone());
    cpi_accounts.push(authority.clone());
    cpi_accounts.push(coin_mint.clone());
    cpi_accounts.push(coin_cfg.clone());
    cpi_accounts.push(genesis_cfg.clone());
    cpi_accounts.push(market_admin.clone());
    cpi_accounts.push(percolator_program.clone());
    cpi_accounts.extend(tail);
    cpi_accounts.push(rewards_program.clone());
    invoke_signed(&ix, &cpi_accounts, &[&signer_seeds])
}

fn process_init_genesis_bootstrap<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let base_mint = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let reward_supply = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);

    let mut ix_data = Vec::with_capacity(9);
    ix_data.push(21u8);
    ix_data.extend_from_slice(&reward_supply.to_le_bytes());
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new_readonly(*base_mint.key, false),
            AccountMeta::new(*genesis_cfg.key, false),
            AccountMeta::new(*genesis_vault.key, false),
            AccountMeta::new(*market_admin.key, false),
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
            coin_mint.clone(),
            coin_cfg.clone(),
            base_mint.clone(),
            genesis_cfg.clone(),
            genesis_vault.clone(),
            market_admin.clone(),
            token_program.clone(),
            rent_sysvar.clone(),
            system_program.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_finalize_genesis<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;

    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new(*genesis_cfg.key, false),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
        ],
        data: vec![25u8],
    };
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            genesis_cfg.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// Forwards rewards `init_genesis_squads` (tag 32). Adapter accounts:
//   payer, authority, rewards_program, coin_mint, coin_cfg, market_admin,
//   create_key, squads_program, program_config, treasury, multisig, system_program
fn process_init_genesis_squads<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let create_key = next_account_info(iter)?;
    let squads_program = next_account_info(iter)?;
    let program_config = next_account_info(iter)?;
    let treasury = next_account_info(iter)?;
    let multisig = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new_readonly(*market_admin.key, false),
            AccountMeta::new_readonly(*create_key.key, false),
            AccountMeta::new_readonly(*squads_program.key, false),
            AccountMeta::new_readonly(*program_config.key, false),
            AccountMeta::new(*treasury.key, false),
            AccountMeta::new(*multisig.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
        ],
        data: vec![REWARDS_IX_INIT_GENESIS_SQUADS],
    };
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            market_admin.clone(),
            create_key.clone(),
            squads_program.clone(),
            program_config.clone(),
            treasury.clone(),
            multisig.clone(),
            system_program.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// Forwards rewards `handover_genesis_squads` (tag 33). Adapter accounts:
//   payer, authority, rewards_program, coin_mint, coin_cfg, genesis_cfg,
//   market_admin, squads_program, multisig, new_authority
fn process_handover_genesis_squads<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let squads_program = next_account_info(iter)?;
    let multisig = next_account_info(iter)?;
    let new_authority = next_account_info(iter)?;

    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new_readonly(*genesis_cfg.key, false),
            AccountMeta::new_readonly(*market_admin.key, false),
            AccountMeta::new_readonly(*squads_program.key, false),
            AccountMeta::new(*multisig.key, false),
            AccountMeta::new_readonly(*new_authority.key, false),
        ],
        data: vec![REWARDS_IX_HANDOVER_GENESIS_SQUADS],
    };
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            genesis_cfg.clone(),
            market_admin.clone(),
            squads_program.clone(),
            multisig.clone(),
            new_authority.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_draw_genesis_surplus<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let mut ix_data = Vec::with_capacity(9);
    ix_data.push(26u8);
    ix_data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*genesis_cfg.key, false),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new(*destination.key, false),
            AccountMeta::new(*genesis_vault.key, false),
            AccountMeta::new_readonly(*market_admin.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: ix_data,
    };
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            genesis_cfg.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            destination.clone(),
            genesis_vault.clone(),
            market_admin.clone(),
            token_program.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_kickstart_genesis_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let backing_domain = read_u8(data)?;
    let backing_expiry_slot = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let mut ix_data = Vec::with_capacity(10);
    ix_data.push(27u8);
    ix_data.push(backing_domain);
    ix_data.extend_from_slice(&backing_expiry_slot.to_le_bytes());
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new(*genesis_cfg.key, false),
            AccountMeta::new_readonly(*market_admin.key, false),
            AccountMeta::new(*market_slab.key, false),
            AccountMeta::new(*genesis_vault.key, false),
            AccountMeta::new(*percolator_vault.key, false),
            AccountMeta::new_readonly(*percolator_vault_pda.key, false),
            AccountMeta::new_readonly(*percolator_program.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: ix_data,
    };
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            genesis_cfg.clone(),
            market_admin.clone(),
            market_slab.clone(),
            genesis_vault.clone(),
            percolator_vault.clone(),
            percolator_vault_pda.clone(),
            percolator_program.clone(),
            token_program.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_recover_genesis_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let tail: Vec<AccountInfo<'a>> = iter.cloned().collect();

    let recovery_kind = read_u8(data)?;
    let domain = read_u8(data)?;
    let amount = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let mut ix_data = Vec::with_capacity(11);
    ix_data.push(28u8);
    ix_data.push(recovery_kind);
    ix_data.push(domain);
    ix_data.extend_from_slice(&amount.to_le_bytes());
    let mut metas = vec![
        AccountMeta::new(*payer.key, true),
        AccountMeta::new_readonly(*authority.key, true),
        AccountMeta::new_readonly(*coin_mint.key, false),
        AccountMeta::new_readonly(*coin_cfg.key, false),
        AccountMeta::new_readonly(*genesis_cfg.key, false),
        AccountMeta::new_readonly(*market_admin.key, false),
        AccountMeta::new(*market_slab.key, false),
        AccountMeta::new(*genesis_vault.key, false),
        AccountMeta::new(*percolator_vault.key, false),
        AccountMeta::new_readonly(*percolator_vault_pda.key, false),
        AccountMeta::new_readonly(*percolator_program.key, false),
        AccountMeta::new_readonly(*token_program.key, false),
    ];
    let mut cpi_accounts = vec![
        payer.clone(),
        authority.clone(),
        coin_mint.clone(),
        coin_cfg.clone(),
        genesis_cfg.clone(),
        market_admin.clone(),
        market_slab.clone(),
        genesis_vault.clone(),
        percolator_vault.clone(),
        percolator_vault_pda.clone(),
        percolator_program.clone(),
        token_program.clone(),
    ];
    for account in tail.iter() {
        metas.push(AccountMeta::new(*account.key, false));
        cpi_accounts.push(account.clone());
    }
    cpi_accounts.push(rewards_program.clone());
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: metas,
        data: ix_data,
    };
    invoke_signed(&ix, &cpi_accounts, &[&signer_seeds])
}

fn process_approve_builder<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let builder_program = next_account_info(iter)?;
    let approval = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let clock = next_account_info(iter)?;

    let code_hash = read_bytes32(data)?;
    let terms_hash = read_bytes32(data)?;
    let enabled = read_u8(data)?;
    if enabled > 1 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);
    let mut ix_data = Vec::with_capacity(66);
    ix_data.push(31u8);
    ix_data.extend_from_slice(&code_hash);
    ix_data.extend_from_slice(&terms_hash);
    ix_data.push(enabled);
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new_readonly(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new_readonly(*builder_program.key, false),
            AccountMeta::new(*approval.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
            AccountMeta::new_readonly(*clock.key, false),
        ],
        data: ix_data,
    };
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            builder_program.clone(),
            approval.clone(),
            system_program.clone(),
            clock.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_mint_reward<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;

    let amount = read_u64(data)?;
    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);

    let mut ix_data = Vec::with_capacity(9);
    ix_data.push(8u8);
    ix_data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new(*destination.key, false),
            AccountMeta::new_readonly(*mint_authority.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
            AccountMeta::new_readonly(*genesis_cfg.key, false),
        ],
        data: ix_data,
    };

    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            destination.clone(),
            mint_authority.clone(),
            token_program.clone(),
            genesis_cfg.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}

fn process_transfer_mint_authority<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let rewards_program = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let new_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;

    let bump = verify_authority_controller(
        program_id,
        payer,
        authority,
        rewards_program.key,
        coin_mint.key,
    )?;
    let bump_bytes = [bump];
    let signer_seeds = authority_signer_seeds(rewards_program.key, coin_mint.key, &bump_bytes);

    let ix = Instruction {
        program_id: *rewards_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new_readonly(*authority.key, true),
            AccountMeta::new(*coin_mint.key, false),
            AccountMeta::new_readonly(*coin_cfg.key, false),
            AccountMeta::new_readonly(*mint_authority.key, false),
            AccountMeta::new_readonly(*new_authority.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
            AccountMeta::new_readonly(*genesis_cfg.key, false),
        ],
        data: vec![10u8],
    };

    invoke_signed(
        &ix,
        &[
            payer.clone(),
            authority.clone(),
            coin_mint.clone(),
            coin_cfg.clone(),
            mint_authority.clone(),
            new_authority.clone(),
            token_program.clone(),
            genesis_cfg.clone(),
            rewards_program.clone(),
        ],
        &[&signer_seeds],
    )
}
