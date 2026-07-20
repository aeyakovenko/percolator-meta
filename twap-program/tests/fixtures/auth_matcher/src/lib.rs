#![allow(unexpected_cfgs)]

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};

entrypoint!(process_instruction);

const MATCHER_ABI_VERSION: u32 = 3;
const MATCHER_RETURN_VALID: u32 = 1;
const CONTEXT_STATE_OFFSET: usize = 64;
const CONTEXT_LEN: usize = CONTEXT_STATE_OFFSET + 33;

fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    match data.first() {
        Some(2) => initialize(program_id, accounts),
        Some(0) => fill(program_id, accounts, data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn initialize(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let accounts = &mut accounts.iter();
    let lp_owner = next_account_info(accounts)?;
    let delegate = next_account_info(accounts)?;
    let context = next_account_info(accounts)?;
    let percolator_program = next_account_info(accounts)?;
    let market = next_account_info(accounts)?;
    let lp_portfolio = next_account_info(accounts)?;
    if accounts.next().is_some()
        || !lp_owner.is_signer
        || !context.is_writable
        || context.owner != program_id
        || context.data_len() < CONTEXT_LEN
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let expected_delegate = Pubkey::find_program_address(
        &[
            b"matcher",
            market.key.as_ref(),
            lp_portfolio.key.as_ref(),
            lp_owner.key.as_ref(),
            program_id.as_ref(),
            context.key.as_ref(),
        ],
        percolator_program.key,
    )
    .0;
    if expected_delegate != *delegate.key {
        return Err(ProgramError::InvalidSeeds);
    }

    let mut state = context.try_borrow_mut_data()?;
    if state[CONTEXT_STATE_OFFSET] != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    state[CONTEXT_STATE_OFFSET] = 1;
    state[CONTEXT_STATE_OFFSET + 1..CONTEXT_LEN].copy_from_slice(delegate.key.as_ref());
    Ok(())
}

fn fill(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if data.len() < 67 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let accounts = &mut accounts.iter();
    let delegate = next_account_info(accounts)?;
    let context = next_account_info(accounts)?;
    if accounts.next().is_some()
        || !delegate.is_signer
        || !context.is_writable
        || context.owner != program_id
        || context.data_len() < CONTEXT_LEN
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let mut state = context.try_borrow_mut_data()?;
    if state[CONTEXT_STATE_OFFSET] != 1
        || state[CONTEXT_STATE_OFFSET + 1..CONTEXT_LEN] != delegate.key.as_ref()[..]
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let request_id = u64::from_le_bytes(data[1..9].try_into().unwrap());
    let asset_index = u16::from_le_bytes(data[9..11].try_into().unwrap()) as u64;
    let lp_account_id = u64::from_le_bytes(data[11..19].try_into().unwrap());
    let oracle_price = u64::from_le_bytes(data[19..27].try_into().unwrap());
    let requested_size = i128::from_le_bytes(data[27..43].try_into().unwrap());

    state[0..4].copy_from_slice(&MATCHER_ABI_VERSION.to_le_bytes());
    state[4..8].copy_from_slice(&MATCHER_RETURN_VALID.to_le_bytes());
    state[8..16].copy_from_slice(&oracle_price.to_le_bytes());
    state[16..32].copy_from_slice(&requested_size.to_le_bytes());
    state[32..40].copy_from_slice(&request_id.to_le_bytes());
    state[40..48].copy_from_slice(&lp_account_id.to_le_bytes());
    state[48..56].copy_from_slice(&oracle_price.to_le_bytes());
    state[56..64].copy_from_slice(&asset_index.to_le_bytes());
    Ok(())
}
