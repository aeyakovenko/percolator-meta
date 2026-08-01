#![allow(unexpected_cfgs)]

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};

entrypoint!(process_instruction);

const INIT_CONTEXT: u8 = 2;
const MATCH_SINGLE: u8 = 0;
const RESPONSE_LEN: usize = 64;
const CONTEXT_STATE_OFFSET: usize = RESPONSE_LEN;
const CONTEXT_LEN: usize = CONTEXT_STATE_OFFSET + 1 + 32;
const MATCH_REQUEST_LEN: usize = 67;

fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    match data.first().copied() {
        Some(INIT_CONTEXT) => initialize_context(program_id, accounts),
        Some(MATCH_SINGLE) => match_single(program_id, accounts, data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn initialize_context(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let accounts = &mut accounts.iter();
    let lp_owner = next_account_info(accounts)?;
    let matcher_delegate = next_account_info(accounts)?;
    let context = next_account_info(accounts)?;
    let percolator_program = next_account_info(accounts)?;
    let market = next_account_info(accounts)?;
    let lp_portfolio = next_account_info(accounts)?;

    if !lp_owner.is_signer
        || !context.is_writable
        || context.owner != program_id
        || context.data_len() < CONTEXT_LEN
        || !percolator_program.executable
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
    if expected_delegate != *matcher_delegate.key {
        return Err(ProgramError::InvalidSeeds);
    }

    let mut context_data = context.try_borrow_mut_data()?;
    if context_data[CONTEXT_STATE_OFFSET] != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    context_data[CONTEXT_STATE_OFFSET] = 1;
    context_data[CONTEXT_STATE_OFFSET + 1..CONTEXT_LEN]
        .copy_from_slice(matcher_delegate.key.as_ref());
    Ok(())
}

fn match_single(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if data.len() != MATCH_REQUEST_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }

    let accounts = &mut accounts.iter();
    let matcher_delegate = next_account_info(accounts)?;
    let context = next_account_info(accounts)?;
    if !matcher_delegate.is_signer
        || !context.is_writable
        || context.owner != program_id
        || context.data_len() < CONTEXT_LEN
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let mut context_data = context.try_borrow_mut_data()?;
    if context_data[CONTEXT_STATE_OFFSET] != 1
        || context_data[CONTEXT_STATE_OFFSET + 1..CONTEXT_LEN] != matcher_delegate.key.as_ref()[..]
    {
        return Err(ProgramError::InvalidAccountData);
    }

    let request_id = &data[1..9];
    let asset_index = u16::from_le_bytes([data[9], data[10]]) as u64;
    let lp_nonce = &data[11..19];
    let oracle_price = &data[19..27];
    let requested_size = &data[27..43];

    let response = &mut context_data[..RESPONSE_LEN];
    response.fill(0);
    response[0..4].copy_from_slice(&3u32.to_le_bytes());
    response[4..8].copy_from_slice(&1u32.to_le_bytes());
    response[8..16].copy_from_slice(oracle_price);
    response[16..32].copy_from_slice(requested_size);
    response[32..40].copy_from_slice(request_id);
    response[40..48].copy_from_slice(lp_nonce);
    response[48..56].copy_from_slice(oracle_price);
    response[56..64].copy_from_slice(&asset_index.to_le_bytes());
    Ok(())
}
