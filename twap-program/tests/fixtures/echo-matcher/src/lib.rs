use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    program_error::ProgramError,
    pubkey::Pubkey,
};

solana_program::entrypoint!(process_instruction);

const MATCHER_ABI_VERSION: u32 = 3;
const FLAG_VALID: u32 = 1;
const REQUEST_LEN: usize = 67;
const RETURN_LEN: usize = 64;

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if data.len() != REQUEST_LEN || data[0] != 0 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let accounts = &mut accounts.iter();
    let delegate = next_account_info(accounts)?;
    let context = next_account_info(accounts)?;
    if !delegate.is_signer || !context.is_writable || context.owner != program_id {
        return Err(ProgramError::InvalidAccountData);
    }

    let req_id = u64::from_le_bytes(data[1..9].try_into().unwrap());
    let asset_index = u16::from_le_bytes(data[9..11].try_into().unwrap());
    let lp_account_id = u64::from_le_bytes(data[11..19].try_into().unwrap());
    let oracle_price = u64::from_le_bytes(data[19..27].try_into().unwrap());
    let requested_size = i128::from_le_bytes(data[27..43].try_into().unwrap());
    if oracle_price == 0 || requested_size == 0 || requested_size == i128::MIN {
        return Err(ProgramError::InvalidInstructionData);
    }

    let mut output = context.try_borrow_mut_data()?;
    if output.len() < RETURN_LEN {
        return Err(ProgramError::AccountDataTooSmall);
    }
    output[..RETURN_LEN].fill(0);
    output[0..4].copy_from_slice(&MATCHER_ABI_VERSION.to_le_bytes());
    output[4..8].copy_from_slice(&FLAG_VALID.to_le_bytes());
    output[8..16].copy_from_slice(&oracle_price.to_le_bytes());
    output[16..32].copy_from_slice(&requested_size.to_le_bytes());
    output[32..40].copy_from_slice(&req_id.to_le_bytes());
    output[40..48].copy_from_slice(&lp_account_id.to_le_bytes());
    output[48..56].copy_from_slice(&oracle_price.to_le_bytes());
    output[56..64].copy_from_slice(&u64::from(asset_index).to_le_bytes());
    Ok(())
}
