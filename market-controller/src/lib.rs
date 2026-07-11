//! Stateless, deny-by-default Percolator market controller.
//!
//! A controller PDA permanently holds `marketauth`. Governance can make it sign
//! only a fixed set of lifecycle and policy instructions. Generic value movement
//! and all authority mutation tags are absent by construction. Fixed shutdown and
//! resolved paths can return backing or insurance only to its recorded provider or
//! retain controller-owned protocol insurance for terminal governance reclaim.
//! Permissionless terminal cleanup can deregister only a resolved empty portfolio;
//! its rent returns to the market slab and no token destination is accepted.
//! Terminal cleanup runs only after Percolator proves every attributed balance is
//! zero.
#![no_std]
extern crate alloc;

#[allow(unused_imports)]
use alloc::format;
use alloc::vec;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    declare_id,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
};

declare_id!("3ueoyr1JepT2DvPxh8LrhdJZ6YsL2sT9Sm7y3TfNyfi9");

const CONTROLLER_SEED: &[u8] = b"market-controller";
const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SUBLEDGER_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("Sub1edger1111111111111111111111111111111111");

const IX_PROXY_ADMIN: u8 = 0;
const IX_INIT_MARKET: u8 = 1;
const IX_GRANT_GENESIS_POOL: u8 = 2;
const IX_ACCEPT_MARKET_AUTHORITY: u8 = 3;
const IX_DONATE_INSURANCE: u8 = 4;
const IX_CLOSE_MARKET_AND_RECLAIM: u8 = 5;
const IX_RETURN_SHUTDOWN_BACKING: u8 = 6;
const IX_RETURN_RESOLVED_ASSET0_BACKING: u8 = 7;
const IX_RETURN_SHUTDOWN_INSURANCE: u8 = 8;
const IX_RETURN_RESOLVED_ASSET_INSURANCE: u8 = 9;
const IX_RETURN_RESOLVED_ASSET_BACKING: u8 = 10;
const IX_CLOSE_RESOLVED_PORTFOLIO: u8 = 11;

const PERC_IX_INIT_MARKET: u8 = 0;
const PERC_IX_CLOSE_PORTFOLIO: u8 = 8;
const PERC_IX_TOP_UP_INSURANCE: u8 = 9;
const PERC_IX_CLOSE_SLAB: u8 = 13;
const PERC_IX_UPDATE_AUTHORITY: u8 = 32;
const PERC_IX_UPDATE_ASSET_LIFECYCLE: u8 = 40;
const PERC_IX_WITHDRAW_BACKING: u8 = 50;
const PERC_IX_WITHDRAW_BACKING_EARNINGS: u8 = 52;
const PERC_IX_WITHDRAW_INSURANCE_ASSET: u8 = 57;
const PERC_IX_UPDATE_ASSET_AUTHORITY: u8 = 65;
const ASSET_ACTION_ACTIVATE: u8 = 0;
const UPDATE_ASSET_LIFECYCLE_LEN: usize = 148;
const ACTIVATE_INSURANCE_AUTHORITY_OFFSET: usize = 20;
const ACTIVATE_INSURANCE_OPERATOR_OFFSET: usize = 52;
const ASSET_AUTH_INSURANCE: u8 = 1;
const ASSET_AUTH_INSURANCE_OPERATOR: u8 = 2;
const ASSET_AUTH_BACKING_BUCKET: u8 = 3;
const ASSET_AUTH_ORACLE: u8 = 4;
const SUBLEDGER_IX_ACCEPT_OPERATOR: u8 = 7;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub fn controller_address(
    governance: &Pubkey,
    market: &Pubkey,
    percolator_program: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            CONTROLLER_SEED,
            governance.as_ref(),
            market.as_ref(),
            percolator_program.as_ref(),
        ],
        &id(),
    )
}

fn controller_bump(
    program_id: &Pubkey,
    governance: &AccountInfo,
    controller: &AccountInfo,
    market: &AccountInfo,
    percolator_program: &AccountInfo,
) -> Result<u8, ProgramError> {
    if !percolator_program.executable || market.owner != percolator_program.key {
        return Err(ProgramError::IncorrectProgramId);
    }
    let (expected, bump) = Pubkey::find_program_address(
        &[
            CONTROLLER_SEED,
            governance.key.as_ref(),
            market.key.as_ref(),
            percolator_program.key.as_ref(),
        ],
        program_id,
    );
    if *controller.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    Ok(bump)
}

fn signer_seeds<'a>(
    governance: &'a Pubkey,
    market: &'a Pubkey,
    percolator_program: &'a Pubkey,
    bump: &'a [u8; 1],
) -> [&'a [u8]; 5] {
    [
        CONTROLLER_SEED,
        governance.as_ref(),
        market.as_ref(),
        percolator_program.as_ref(),
        bump,
    ]
}

/// Exact pinned-v16 generic governance surface. Every value mover, authority
/// mutation, trader/portfolio operation, oracle push, recovery accounting
/// operation, and raw CloseSlab call is intentionally absent. CloseSlab is exposed
/// only through the fixed terminal cleanup instruction below.
fn admin_tag_allowed(tag: u8) -> bool {
    matches!(
        tag,
        19 // ResolveMarket
            | 34 // ConfigureHybridOracle
            | 35 // ConfigureEwmaMark
            | 37 // UpdateLiquidationFeePolicy
            | 38 // ConfigurePermissionlessResolve
            | 40 // UpdateAssetLifecycle (activate/drain/retire/shutdown)
            | 49 // UpdateMaintenanceFeePolicy
            | 51 // UpdateBackingFeePolicy
            | 55 // UpdateTradeFeePolicy
            | 58 // UpdateFeeRedirectPolicy
            | 62 // ConfigureAuthMark
            | 69 // RestartAssetOracle
    )
}

fn validate_admin_instruction_data(data: &[u8]) -> ProgramResult {
    if data.first().copied() != Some(PERC_IX_UPDATE_ASSET_LIFECYCLE)
        || data.get(1).copied() != Some(ASSET_ACTION_ACTIVATE)
    {
        return Ok(());
    }
    if data.len() != UPDATE_ASSET_LIFECYCLE_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let insurance_authority = data
        .get(ACTIVATE_INSURANCE_AUTHORITY_OFFSET..ACTIVATE_INSURANCE_OPERATOR_OFFSET)
        .ok_or(ProgramError::InvalidInstructionData)?;
    let insurance_operator = data
        .get(ACTIVATE_INSURANCE_OPERATOR_OFFSET..ACTIVATE_INSURANCE_OPERATOR_OFFSET + 32)
        .ok_or(ProgramError::InvalidInstructionData)?;
    if insurance_authority != insurance_operator {
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok(())
}

pub fn process_instruction<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    instruction_data: &[u8],
) -> ProgramResult {
    let (tag, data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match *tag {
        IX_PROXY_ADMIN => process_proxy_admin(program_id, accounts, data),
        IX_INIT_MARKET => process_init_market(program_id, accounts, data),
        IX_GRANT_GENESIS_POOL => process_grant_genesis_pool(program_id, accounts, data),
        IX_ACCEPT_MARKET_AUTHORITY => process_accept_market_authority(program_id, accounts, data),
        IX_DONATE_INSURANCE => process_donate_insurance(program_id, accounts, data),
        IX_CLOSE_MARKET_AND_RECLAIM => process_close_market_and_reclaim(program_id, accounts, data),
        IX_RETURN_SHUTDOWN_BACKING => process_return_shutdown_backing(program_id, accounts, data),
        IX_RETURN_RESOLVED_ASSET0_BACKING => {
            process_return_resolved_asset0_backing(program_id, accounts, data)
        }
        IX_RETURN_SHUTDOWN_INSURANCE => {
            process_return_shutdown_insurance(program_id, accounts, data)
        }
        IX_RETURN_RESOLVED_ASSET_INSURANCE => {
            process_return_resolved_asset_insurance(program_id, accounts, data)
        }
        IX_RETURN_RESOLVED_ASSET_BACKING => {
            process_return_resolved_asset_backing(program_id, accounts, data)
        }
        IX_CLOSE_RESOLVED_PORTFOLIO => process_close_resolved_portfolio(program_id, accounts, data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

// proxy_admin accounts:
// [governance(signer), controller_pda, market(w), percolator_program, tail...]
// data: exact raw Percolator instruction bytes.
fn process_proxy_admin<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    let perc_tag = data
        .first()
        .copied()
        .ok_or(ProgramError::InvalidInstructionData)?;
    if !admin_tag_allowed(perc_tag) {
        return Err(ProgramError::InvalidInstructionData);
    }
    validate_admin_instruction_data(data)?;
    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if !governance.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;

    let tail: alloc::vec::Vec<AccountInfo<'a>> = iter.cloned().collect();
    let controller_meta = if controller.is_writable {
        AccountMeta::new(*controller.key, true)
    } else {
        AccountMeta::new_readonly(*controller.key, true)
    };
    let mut metas = alloc::vec::Vec::with_capacity(2 + tail.len());
    metas.push(controller_meta);
    metas.push(AccountMeta::new(*market.key, false));
    for account in tail.iter() {
        let meta = if account.is_writable {
            AccountMeta::new(*account.key, account.is_signer)
        } else {
            AccountMeta::new_readonly(*account.key, account.is_signer)
        };
        metas.push(meta);
    }
    let mut cpi_accounts = alloc::vec::Vec::with_capacity(3 + tail.len());
    cpi_accounts.push(controller.clone());
    cpi_accounts.push(market.clone());
    cpi_accounts.extend(tail);
    cpi_accounts.push(percolator_program.clone());
    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: metas,
            data: data.to_vec(),
        },
        &cpi_accounts,
        &[&seeds],
    )
}

fn validate_reclaim_token_accounts(
    controller: &AccountInfo,
    governance: &AccountInfo,
    transit: &AccountInfo,
    destination: &AccountInfo,
) -> ProgramResult {
    if transit.owner != &spl_token::ID || destination.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    if transit.key == destination.key {
        return Err(ProgramError::InvalidAccountData);
    }
    let transit_state = spl_token::state::Account::unpack(&transit.try_borrow_data()?)?;
    let destination_state = spl_token::state::Account::unpack(&destination.try_borrow_data()?)?;
    let canonical_transit = Pubkey::find_program_address(
        &[
            controller.key.as_ref(),
            spl_token::ID.as_ref(),
            transit_state.mint.as_ref(),
        ],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0;
    if transit_state.state != spl_token::state::AccountState::Initialized
        || destination_state.state != spl_token::state::AccountState::Initialized
        || *transit.key != canonical_transit
        || transit_state.owner != *controller.key
        || destination_state.owner != *governance.key
        || transit_state.mint != destination_state.mint
        || transit_state.delegate.is_some()
        || transit_state.delegated_amount != 0
        || transit_state.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn forward_and_close_token_account<'a>(
    controller: &AccountInfo<'a>,
    lamport_destination: &AccountInfo<'a>,
    transit: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
) -> ProgramResult {
    let amount = spl_token::state::Account::unpack(&transit.try_borrow_data()?)?.amount;
    if amount > 0 {
        invoke_signed(
            &spl_token::instruction::transfer(
                token_program.key,
                transit.key,
                destination.key,
                controller.key,
                &[],
                amount,
            )?,
            &[
                transit.clone(),
                destination.clone(),
                controller.clone(),
                token_program.clone(),
            ],
            &[signer_seeds],
        )?;
    }
    invoke_signed(
        &spl_token::instruction::close_account(
            token_program.key,
            transit.key,
            lamport_destination.key,
            controller.key,
            &[],
        )?,
        &[
            transit.clone(),
            lamport_destination.clone(),
            controller.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )
}

fn forward_exact_and_close_if_empty<'a>(
    controller: &AccountInfo<'a>,
    lamport_destination: &AccountInfo<'a>,
    transit: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
    amount: u64,
) -> ProgramResult {
    let available = spl_token::state::Account::unpack(&transit.try_borrow_data()?)?.amount;
    if available < amount {
        return Err(ProgramError::InsufficientFunds);
    }
    if amount > 0 {
        invoke_signed(
            &spl_token::instruction::transfer(
                token_program.key,
                transit.key,
                destination.key,
                controller.key,
                &[],
                amount,
            )?,
            &[
                transit.clone(),
                destination.clone(),
                controller.clone(),
                token_program.clone(),
            ],
            &[signer_seeds],
        )?;
    }
    // A prior fixed cleanup may have retained protocol value in this canonical
    // controller account. It belongs to terminal reclaim, not the current provider.
    if spl_token::state::Account::unpack(&transit.try_borrow_data()?)?.amount != 0 {
        return Ok(());
    }
    invoke_signed(
        &spl_token::instruction::close_account(
            token_program.key,
            transit.key,
            lamport_destination.key,
            controller.key,
            &[],
        )?,
        &[
            transit.clone(),
            lamport_destination.clone(),
            controller.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )
}

fn validate_provider_return_token_accounts(
    controller: &AccountInfo,
    provider: &Pubkey,
    transit: &AccountInfo,
    destination: &AccountInfo,
) -> ProgramResult {
    if transit.owner != &spl_token::ID
        || destination.owner != &spl_token::ID
        || transit.key == destination.key
        || provider == controller.key
    {
        return Err(ProgramError::IllegalOwner);
    }
    let transit_state = spl_token::state::Account::unpack(&transit.try_borrow_data()?)?;
    let destination_state = spl_token::state::Account::unpack(&destination.try_borrow_data()?)?;
    let canonical_transit = Pubkey::find_program_address(
        &[
            controller.key.as_ref(),
            spl_token::ID.as_ref(),
            transit_state.mint.as_ref(),
        ],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0;
    let canonical_destination = Pubkey::find_program_address(
        &[
            provider.as_ref(),
            spl_token::ID.as_ref(),
            transit_state.mint.as_ref(),
        ],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0;
    // The address, mint, and initialized state are the immutable routing boundary.
    // Requiring the mutable token owner to remain `provider` lets that provider
    // reassign the account to a dead key and permanently block terminal cleanup.
    if transit_state.state != spl_token::state::AccountState::Initialized
        || destination_state.state != spl_token::state::AccountState::Initialized
        || *transit.key != canonical_transit
        || *destination.key != canonical_destination
        || transit_state.owner != *controller.key
        || transit_state.mint != destination_state.mint
        || transit_state.delegate.is_some()
        || transit_state.delegated_amount != 0
        || transit_state.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn validate_controller_insurance_transit(
    controller: &AccountInfo,
    transit: &AccountInfo,
    destination: &AccountInfo,
) -> ProgramResult {
    if transit.key != destination.key || transit.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let transit_state = spl_token::state::Account::unpack(&transit.try_borrow_data()?)?;
    let canonical_transit = Pubkey::find_program_address(
        &[
            controller.key.as_ref(),
            spl_token::ID.as_ref(),
            transit_state.mint.as_ref(),
        ],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0;
    if transit_state.state != spl_token::state::AccountState::Initialized
        || *transit.key != canonical_transit
        || transit_state.owner != *controller.key
        || transit_state.delegate.is_some()
        || transit_state.delegated_amount != 0
        || transit_state.close_authority.is_some()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn withdraw_backing_earnings<'a>(
    controller: &AccountInfo<'a>,
    market: &AccountInfo<'a>,
    backing_ledger: &AccountInfo<'a>,
    controller_transit: &AccountInfo<'a>,
    percolator_vault: &AccountInfo<'a>,
    vault_authority: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    percolator_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
    domain: u16,
    amount: u128,
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let mut ix_data = vec![PERC_IX_WITHDRAW_BACKING_EARNINGS];
    ix_data.extend_from_slice(&domain.to_le_bytes());
    ix_data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
                AccountMeta::new(*backing_ledger.key, false),
                AccountMeta::new(*controller_transit.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*vault_authority.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: ix_data,
        },
        &[
            controller.clone(),
            market.clone(),
            backing_ledger.clone(),
            controller_transit.clone(),
            percolator_vault.clone(),
            vault_authority.clone(),
            token_program.clone(),
            percolator_program.clone(),
        ],
        &[signer_seeds],
    )
}

#[allow(clippy::too_many_arguments)]
fn withdraw_backing_principal<'a>(
    controller: &AccountInfo<'a>,
    market: &AccountInfo<'a>,
    controller_transit: &AccountInfo<'a>,
    percolator_vault: &AccountInfo<'a>,
    vault_authority: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    percolator_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
    domain: u16,
    amount: u128,
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let mut ix_data = vec![PERC_IX_WITHDRAW_BACKING];
    ix_data.extend_from_slice(&domain.to_le_bytes());
    ix_data.extend_from_slice(&amount.to_le_bytes());
    let metas = vec![
        AccountMeta::new_readonly(*controller.key, true),
        AccountMeta::new(*market.key, false),
        AccountMeta::new(*controller_transit.key, false),
        AccountMeta::new(*percolator_vault.key, false),
        AccountMeta::new_readonly(*vault_authority.key, false),
        AccountMeta::new_readonly(*token_program.key, false),
    ];
    let infos = vec![
        controller.clone(),
        market.clone(),
        controller_transit.clone(),
        percolator_vault.clone(),
        vault_authority.clone(),
        token_program.clone(),
        percolator_program.clone(),
    ];
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: metas,
            data: ix_data,
        },
        &infos,
        &[signer_seeds],
    )
}

// return_shutdown_backing accounts:
// [governance, controller_pda, market(w), provider_canonical_ata(w),
//  controller_canonical_ata(w), percolator_vault(w), vault_authority,
//  controller_backing_ledger(w), percolator_program, token_program]
// data: domain(u16) | principal(u128) | earnings(u128)
//
// Permissionless after Percolator's own asset-shutdown delay and empty-state checks.
// The controller can exercise marketauth's shutdown override, but neither governance
// nor the caller chooses the recipient: the attributed withdrawal and, when the
// transit account is empty, its rent go to the canonical ATA of the backing authority
// recorded in the asset profile. Earnings are returned first because Percolator
// refuses a final-principal exit while earnings remain.
fn process_return_shutdown_backing<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 34 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let domain = u16::from_le_bytes(data[0..2].try_into().unwrap());
    let principal = u128::from_le_bytes(data[2..18].try_into().unwrap());
    let earnings = u128::from_le_bytes(data[18..34].try_into().unwrap());
    if principal == 0 && earnings == 0 {
        return Err(ProgramError::InvalidArgument);
    }

    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let provider_destination = next_account_info(iter)?;
    let controller_transit = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let backing_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market.is_writable
        || !provider_destination.is_writable
        || !controller_transit.is_writable
        || !percolator_vault.is_writable
        || !backing_ledger.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if *token_program.key != spl_token::ID || backing_ledger.owner != percolator_program.key {
        return Err(ProgramError::IncorrectProgramId);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let provider = {
        let market_data = market.try_borrow_data()?;
        let authority = percolator_accounting::read_asset_backing_authority(
            &market_data,
            usize::from(domain / 2),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        if authority == [0u8; 32] {
            return Err(ProgramError::InvalidAccountData);
        }
        Pubkey::new_from_array(authority)
    };
    validate_provider_return_token_accounts(
        controller,
        &provider,
        controller_transit,
        provider_destination,
    )?;

    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    let return_amount = principal
        .checked_add(earnings)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let return_amount =
        u64::try_from(return_amount).map_err(|_| ProgramError::ArithmeticOverflow)?;
    withdraw_backing_earnings(
        controller,
        market,
        backing_ledger,
        controller_transit,
        percolator_vault,
        vault_authority,
        token_program,
        percolator_program,
        &seeds,
        domain,
        earnings,
    )?;
    withdraw_backing_principal(
        controller,
        market,
        controller_transit,
        percolator_vault,
        vault_authority,
        token_program,
        percolator_program,
        &seeds,
        domain,
        principal,
    )?;
    forward_exact_and_close_if_empty(
        controller,
        provider_destination,
        controller_transit,
        provider_destination,
        token_program,
        &seeds,
        return_amount,
    )
}

// return_shutdown_insurance accounts:
// [governance, controller_pda, market(w), authority_canonical_ata(w),
//  controller_canonical_ata(w), percolator_vault(w), vault_authority,
//  controller_insurance_ledger(w), percolator_program, token_program]
// data: asset_index(u16)
//
// Permissionless only through Percolator's secondary-asset marketauth shutdown
// override. The amount is the complete asset-local balance read from the pinned
// slab, and both tokens and transit rent go only to the canonical ATA of the
// recorded insurance authority. Rejecting a controller-owned live operator keeps
// this path unavailable before Percolator's shutdown delay has matured.
fn process_return_shutdown_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 2 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let asset_index = u16::from_le_bytes(data.try_into().unwrap());
    if asset_index == 0 {
        return Err(ProgramError::InvalidArgument);
    }

    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let provider_destination = next_account_info(iter)?;
    let controller_transit = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let insurance_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market.is_writable
        || !provider_destination.is_writable
        || !controller_transit.is_writable
        || !percolator_vault.is_writable
        || !insurance_ledger.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if *token_program.key != spl_token::ID || insurance_ledger.owner != percolator_program.key {
        return Err(ProgramError::IncorrectProgramId);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let (provider, amount) = {
        let market_data = market.try_borrow_data()?;
        if percolator_accounting::read_market_authority(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != controller.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let authority = percolator_accounting::read_asset_insurance_authority(
            &market_data,
            usize::from(asset_index),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        let operator = percolator_accounting::read_asset_insurance_operator(
            &market_data,
            usize::from(asset_index),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        let amount = percolator_accounting::read_asset_insurance_remaining(
            &market_data,
            usize::from(asset_index),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        if authority == [0u8; 32]
            || operator == [0u8; 32]
            || operator == controller.key.to_bytes()
            || amount == 0
        {
            return Err(ProgramError::InvalidAccountData);
        }
        (Pubkey::new_from_array(authority), amount)
    };
    validate_provider_return_token_accounts(
        controller,
        &provider,
        controller_transit,
        provider_destination,
    )?;

    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    let mut ix_data = vec![PERC_IX_WITHDRAW_INSURANCE_ASSET];
    ix_data.extend_from_slice(&asset_index.to_le_bytes());
    ix_data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
                AccountMeta::new(*controller_transit.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*vault_authority.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
                AccountMeta::new(*insurance_ledger.key, false),
            ],
            data: ix_data,
        },
        &[
            controller.clone(),
            market.clone(),
            controller_transit.clone(),
            percolator_vault.clone(),
            vault_authority.clone(),
            token_program.clone(),
            insurance_ledger.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )?;
    let return_amount = u64::try_from(amount).map_err(|_| ProgramError::ArithmeticOverflow)?;
    forward_exact_and_close_if_empty(
        controller,
        provider_destination,
        controller_transit,
        provider_destination,
        token_program,
        &seeds,
        return_amount,
    )
}

fn rotate_asset_role_to_controller<'a>(
    controller: &AccountInfo<'a>,
    market: &AccountInfo<'a>,
    percolator_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
    asset_index: u16,
    kind: u8,
) -> ProgramResult {
    let mut data = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
    data.extend_from_slice(&asset_index.to_le_bytes());
    data.push(kind);
    data.extend_from_slice(controller.key.as_ref());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
            ],
            data,
        },
        &[
            controller.clone(),
            controller.clone(),
            market.clone(),
            percolator_program.clone(),
        ],
        &[signer_seeds],
    )
}

// return_resolved_asset_insurance accounts:
// [governance, controller_pda, market(w), authority_canonical_ata(w),
//  controller_canonical_ata(w), percolator_vault(w), vault_authority,
//  controller_insurance_ledger(w), percolator_program, token_program]
// data: asset_index(u16)
//
// Global permissionless resolution may race the live shutdown return above. For an
// external provider, this fixed path rotates only insurance_authority to the
// controller, withdraws the exact asset-local remainder read from the pinned
// slab, and forwards it to the outgoing authority's canonical ATA. This includes
// asset 0 only when the controller still holds its asset-admin rotation key.
// Controller-owned protocol insurance stays in the canonical controller ATA for
// the existing terminal governance reclaim. Any failed operation rolls back.
fn process_return_resolved_asset_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 2 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let asset_index = u16::from_le_bytes(data.try_into().unwrap());

    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let provider_destination = next_account_info(iter)?;
    let controller_transit = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let insurance_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market.is_writable
        || !provider_destination.is_writable
        || !controller_transit.is_writable
        || !percolator_vault.is_writable
        || !insurance_ledger.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if *token_program.key != spl_token::ID || insurance_ledger.owner != percolator_program.key {
        return Err(ProgramError::IncorrectProgramId);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let (provider, amount, controller_owned) = {
        let market_data = market.try_borrow_data()?;
        if percolator_accounting::read_market_authority(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != controller.key.to_bytes()
            || !percolator_accounting::market_is_resolved_and_empty(&market_data)
                .map_err(|_| ProgramError::InvalidAccountData)?
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let authority = percolator_accounting::read_asset_insurance_authority(
            &market_data,
            usize::from(asset_index),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        let amount = percolator_accounting::read_asset_insurance_remaining(
            &market_data,
            usize::from(asset_index),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        let controller_owned = authority == controller.key.to_bytes();
        let asset_admin =
            percolator_accounting::read_asset_admin(&market_data, usize::from(asset_index))
                .map_err(|_| ProgramError::InvalidAccountData)?;
        if authority == [0u8; 32]
            || amount == 0
            || (!controller_owned && asset_admin != controller.key.to_bytes())
        {
            return Err(ProgramError::InvalidAccountData);
        }
        (Pubkey::new_from_array(authority), amount, controller_owned)
    };
    if controller_owned {
        validate_controller_insurance_transit(
            controller,
            controller_transit,
            provider_destination,
        )?;
    } else {
        validate_provider_return_token_accounts(
            controller,
            &provider,
            controller_transit,
            provider_destination,
        )?;
    }

    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    if !controller_owned {
        rotate_asset_role_to_controller(
            controller,
            market,
            percolator_program,
            &seeds,
            asset_index,
            ASSET_AUTH_INSURANCE,
        )?;
    }

    let mut ix_data = vec![PERC_IX_WITHDRAW_INSURANCE_ASSET];
    ix_data.extend_from_slice(&asset_index.to_le_bytes());
    ix_data.extend_from_slice(&amount.to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
                AccountMeta::new(*controller_transit.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*vault_authority.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
                AccountMeta::new(*insurance_ledger.key, false),
            ],
            data: ix_data,
        },
        &[
            controller.clone(),
            market.clone(),
            controller_transit.clone(),
            percolator_vault.clone(),
            vault_authority.clone(),
            token_program.clone(),
            insurance_ledger.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )?;
    if controller_owned {
        Ok(())
    } else {
        let return_amount = u64::try_from(amount).map_err(|_| ProgramError::ArithmeticOverflow)?;
        forward_exact_and_close_if_empty(
            controller,
            provider_destination,
            controller_transit,
            provider_destination,
            token_program,
            &seeds,
            return_amount,
        )
    }
}

// return_resolved_asset_backing accounts:
// [governance, controller_pda, market(w), provider_canonical_ata(w),
//  controller_canonical_ata(w), percolator_vault(w), vault_authority,
//  long_backing_ledger(w), short_backing_ledger(w), percolator_program,
//  token_program]
// data: asset_index(u16)
//
// Resolved-mode companion to the shutdown backing return. The controller can
// rotate only the backing role it already administers, and all principal and
// earnings are slab-derived and returned to the outgoing recorded provider.
fn process_return_resolved_asset_backing<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 2 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let asset_index = u16::from_le_bytes(data.try_into().unwrap());
    if asset_index == 0 {
        return Err(ProgramError::InvalidArgument);
    }
    let long_domain = asset_index
        .checked_mul(2)
        .ok_or(ProgramError::InvalidInstructionData)?;
    let short_domain = long_domain
        .checked_add(1)
        .ok_or(ProgramError::InvalidInstructionData)?;

    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let provider_destination = next_account_info(iter)?;
    let controller_transit = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let long_backing_ledger = next_account_info(iter)?;
    let short_backing_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market.is_writable
        || !provider_destination.is_writable
        || !controller_transit.is_writable
        || !percolator_vault.is_writable
        || !long_backing_ledger.is_writable
        || !short_backing_ledger.is_writable
        || long_backing_ledger.key == short_backing_ledger.key
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if *token_program.key != spl_token::ID
        || long_backing_ledger.owner != percolator_program.key
        || short_backing_ledger.owner != percolator_program.key
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let (provider, balances) = {
        let market_data = market.try_borrow_data()?;
        if percolator_accounting::read_market_authority(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != controller.key.to_bytes()
            || !percolator_accounting::market_is_resolved_and_empty(&market_data)
                .map_err(|_| ProgramError::InvalidAccountData)?
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let authority = percolator_accounting::read_asset_backing_authority(
            &market_data,
            usize::from(asset_index),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        let balances = percolator_accounting::read_asset_backing_balances(
            &market_data,
            usize::from(asset_index),
        )
        .map_err(|_| ProgramError::InvalidAccountData)?;
        if authority == [0u8; 32]
            || authority == controller.key.to_bytes()
            || !balances
                .iter()
                .any(|balance| balance.principal_atoms != 0 || balance.earnings_atoms != 0)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        (Pubkey::new_from_array(authority), balances)
    };
    validate_provider_return_token_accounts(
        controller,
        &provider,
        controller_transit,
        provider_destination,
    )?;

    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    let return_amount = balances.iter().try_fold(0u128, |total, balance| {
        total
            .checked_add(balance.principal_atoms)
            .and_then(|value| value.checked_add(balance.earnings_atoms))
            .ok_or(ProgramError::ArithmeticOverflow)
    })?;
    let return_amount =
        u64::try_from(return_amount).map_err(|_| ProgramError::ArithmeticOverflow)?;
    rotate_asset_role_to_controller(
        controller,
        market,
        percolator_program,
        &seeds,
        asset_index,
        ASSET_AUTH_BACKING_BUCKET,
    )?;

    for (domain, balance, backing_ledger) in [
        (long_domain, balances[0], long_backing_ledger),
        (short_domain, balances[1], short_backing_ledger),
    ] {
        withdraw_backing_earnings(
            controller,
            market,
            backing_ledger,
            controller_transit,
            percolator_vault,
            vault_authority,
            token_program,
            percolator_program,
            &seeds,
            domain,
            balance.earnings_atoms,
        )?;
        withdraw_backing_principal(
            controller,
            market,
            controller_transit,
            percolator_vault,
            vault_authority,
            token_program,
            percolator_program,
            &seeds,
            domain,
            balance.principal_atoms,
        )?;
    }
    forward_exact_and_close_if_empty(
        controller,
        provider_destination,
        controller_transit,
        provider_destination,
        token_program,
        &seeds,
        return_amount,
    )
}

// return_resolved_asset0_backing accounts:
// [governance, controller_pda, current_asset_admin(signer unless controller), market(w),
//  provider_canonical_ata(w), controller_canonical_ata(w), percolator_vault(w),
//  vault_authority, long_backing_ledger(w), short_backing_ledger(w),
//  percolator_program, token_program]
//
// Asset 0 cannot enter Percolator's per-asset shutdown state, so marketauth never
// receives the secondary-asset shutdown withdrawal override used above. Once the
// whole market is resolved and empty, this fixed operation atomically moves only
// the backing role to the controller, drains both asset-0 domains using balances
// read from the pinned slab, and forwards everything to the outgoing provider.
// No caller selects a recipient or amount, and any failed CPI rolls back the role
// transition. A constrained current asset-admin program may invoke this instruction
// as a signer; before the first custody handoff the controller signs both roles.
fn process_return_resolved_asset0_backing<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let current_asset_admin = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let provider_destination = next_account_info(iter)?;
    let controller_transit = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let long_backing_ledger = next_account_info(iter)?;
    let short_backing_ledger = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if iter.next().is_some()
        || !market.is_writable
        || !provider_destination.is_writable
        || !controller_transit.is_writable
        || !percolator_vault.is_writable
        || !long_backing_ledger.is_writable
        || !short_backing_ledger.is_writable
        || long_backing_ledger.key == short_backing_ledger.key
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if current_asset_admin.key != controller.key && !current_asset_admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID
        || long_backing_ledger.owner != percolator_program.key
        || short_backing_ledger.owner != percolator_program.key
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let (provider, balances) = {
        let market_data = market.try_borrow_data()?;
        if percolator_accounting::read_market_authority(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != controller.key.to_bytes()
            || !percolator_accounting::market_is_resolved_and_empty(&market_data)
                .map_err(|_| ProgramError::InvalidAccountData)?
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let authority = percolator_accounting::read_asset_backing_authority(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let balances = percolator_accounting::read_asset_backing_balances(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        if authority == [0u8; 32]
            || !balances
                .iter()
                .any(|balance| balance.principal_atoms != 0 || balance.earnings_atoms != 0)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        (Pubkey::new_from_array(authority), balances)
    };
    validate_provider_return_token_accounts(
        controller,
        &provider,
        controller_transit,
        provider_destination,
    )?;

    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    let return_amount = balances.iter().try_fold(0u128, |total, balance| {
        total
            .checked_add(balance.principal_atoms)
            .and_then(|value| value.checked_add(balance.earnings_atoms))
            .ok_or(ProgramError::ArithmeticOverflow)
    })?;
    let return_amount =
        u64::try_from(return_amount).map_err(|_| ProgramError::ArithmeticOverflow)?;
    let mut rotate_data = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
    rotate_data.extend_from_slice(&0u16.to_le_bytes());
    rotate_data.push(ASSET_AUTH_BACKING_BUCKET);
    rotate_data.extend_from_slice(controller.key.as_ref());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*current_asset_admin.key, true),
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
            ],
            data: rotate_data,
        },
        &[
            current_asset_admin.clone(),
            controller.clone(),
            market.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )?;

    for (domain, balance, backing_ledger) in [
        (0u16, balances[0], long_backing_ledger),
        (1u16, balances[1], short_backing_ledger),
    ] {
        withdraw_backing_earnings(
            controller,
            market,
            backing_ledger,
            controller_transit,
            percolator_vault,
            vault_authority,
            token_program,
            percolator_program,
            &seeds,
            domain,
            balance.earnings_atoms,
        )?;
        withdraw_backing_principal(
            controller,
            market,
            controller_transit,
            percolator_vault,
            vault_authority,
            token_program,
            percolator_program,
            &seeds,
            domain,
            balance.principal_atoms,
        )?;
    }
    forward_exact_and_close_if_empty(
        controller,
        provider_destination,
        controller_transit,
        provider_destination,
        token_program,
        &seeds,
        return_amount,
    )
}

// close_resolved_portfolio accounts:
// [governance, controller_pda, market(w), portfolio(w), percolator_program]
//
// Anyone can ask the controller to exercise Percolator's terminal marketauth
// override. Percolator itself requires a resolved market and a genuinely empty
// portfolio, deregisters the materialized account, and returns only its rent to
// the market slab. Residual rewards bind to the dematerialized key and frozen
// recipient, so historical counters cannot become a gate on insurance/backing
// exits. There is no caller-selected amount or destination.
fn process_close_resolved_portfolio<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let portfolio = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if iter.next().is_some() || !market.is_writable || !portfolio.is_writable {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
                AccountMeta::new(*portfolio.key, false),
            ],
            data: vec![PERC_IX_CLOSE_PORTFOLIO],
        },
        &[
            controller.clone(),
            market.clone(),
            portfolio.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )
}

// close_market_and_reclaim accounts:
// [governance(s,w), controller(w), market(w), vault_authority,
//  primary_vault(w), controller_primary_transit(w), governance_primary_dest(w),
//  percolator_program, token_program, system_program,
//  optional secondary_vault(w), controller_secondary_transit(w), governance_secondary_dest(w)]
//
// Percolator's CloseSlab requires its current marketauth to receive the slab rent,
// vault rent, and any raw vault dust. Here marketauth is the stateless controller
// PDA, so exposing CloseSlab through the generic proxy would strand that value.
// This fixed instruction closes only a fully wound-down market (enforced by
// Percolator), forwards each controller canonical ATA's complete balance to a
// governance-owned token account, closes the temporary accounts, and forwards
// every recovered lamport. Forwarding the complete canonical balance makes public
// pre-execution token dust harmless without exposing an arbitrary token sweep.
fn process_close_market_and_reclaim<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let primary_vault = next_account_info(iter)?;
    let primary_transit = next_account_info(iter)?;
    let primary_destination = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let optional: alloc::vec::Vec<AccountInfo<'a>> = iter.cloned().collect();
    let secondary = match optional.as_slice() {
        [] => None,
        [vault, transit, destination] => Some((vault, transit, destination)),
        _ => return Err(ProgramError::InvalidInstructionData),
    };

    if !governance.is_signer || !governance.is_writable {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !controller.is_writable
        || !market.is_writable
        || !primary_vault.is_writable
        || !primary_transit.is_writable
        || !primary_destination.is_writable
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if *token_program.key != spl_token::ID
        || *system_program.key != solana_program::system_program::ID
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    if controller.owner != system_program.key || controller.data_len() != 0 {
        return Err(ProgramError::IllegalOwner);
    }
    if let Some((vault, transit, destination)) = secondary {
        if !vault.is_writable || !transit.is_writable || !destination.is_writable {
            return Err(ProgramError::InvalidAccountData);
        }
        validate_reclaim_token_accounts(controller, governance, transit, destination)?;
    }
    validate_reclaim_token_accounts(controller, governance, primary_transit, primary_destination)?;

    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );

    let mut metas = vec![
        AccountMeta::new(*controller.key, true),
        AccountMeta::new(*market.key, false),
        AccountMeta::new(*primary_vault.key, false),
        AccountMeta::new_readonly(*vault_authority.key, false),
        AccountMeta::new(*primary_transit.key, false),
        AccountMeta::new_readonly(*token_program.key, false),
    ];
    let mut cpi_accounts = vec![
        controller.clone(),
        market.clone(),
        primary_vault.clone(),
        vault_authority.clone(),
        primary_transit.clone(),
        token_program.clone(),
    ];
    if let Some((vault, transit, _)) = secondary {
        metas.push(AccountMeta::new(*vault.key, false));
        metas.push(AccountMeta::new(*transit.key, false));
        cpi_accounts.push(vault.clone());
        cpi_accounts.push(transit.clone());
    }
    cpi_accounts.push(percolator_program.clone());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: metas,
            data: vec![PERC_IX_CLOSE_SLAB],
        },
        &cpi_accounts,
        &[&seeds],
    )?;

    forward_and_close_token_account(
        controller,
        governance,
        primary_transit,
        primary_destination,
        token_program,
        &seeds,
    )?;
    if let Some((_, transit, destination)) = secondary {
        forward_and_close_token_account(
            controller,
            governance,
            transit,
            destination,
            token_program,
            &seeds,
        )?;
    }

    let recovered_lamports = controller.lamports();
    if recovered_lamports > 0 {
        invoke_signed(
            &system_instruction::transfer(controller.key, governance.key, recovered_lamports),
            &[
                controller.clone(),
                governance.clone(),
                system_program.clone(),
            ],
            &[&seeds],
        )?;
    }
    Ok(())
}

// init_market accounts:
// [payer(s), governance, controller_pda, market(w), collateral_mint, percolator_program]
// data: exact raw Percolator InitMarket bytes. Governance need not sign, so market
// creation is permissionless while future controller actions remain governance-gated.
fn process_init_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if data.first().copied() != Some(PERC_IX_INIT_MARKET) {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if !payer.is_signer || iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
                AccountMeta::new_readonly(*collateral_mint.key, false),
            ],
            data: data.to_vec(),
        },
        &[
            controller.clone(),
            market.clone(),
            collateral_mint.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )
}

// grant_genesis_pool accounts:
// [governance(signer), controller_pda, pool, market(w), percolator_program,
//  subledger_program]
fn process_grant_genesis_pool<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let pool = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let subledger_program = next_account_info(iter)?;
    if !governance.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *subledger_program.key != SUBLEDGER_PROGRAM_ID || !subledger_program.executable {
        return Err(ProgramError::IncorrectProgramId);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    {
        let market_data = market.try_borrow_data()?;
        let remaining = percolator_accounting::read_asset_insurance_remaining(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        if remaining != 0 {
            let authority = percolator_accounting::read_asset_insurance_authority(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
            let operator = percolator_accounting::read_asset_insurance_operator(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
            if authority != controller.key.to_bytes() || operator != controller.key.to_bytes() {
                return Err(ProgramError::InvalidAccountData);
            }
        }
    }
    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    // Leave only the oracle role on governance before asset_admin moves to the
    // pool. The current oracle holder can later self-rotate to an approved builder,
    // while no governance-controlled role can move insurance or backing.
    let mut oracle_ix_data = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
    oracle_ix_data.extend_from_slice(&0u16.to_le_bytes());
    oracle_ix_data.push(ASSET_AUTH_ORACLE);
    oracle_ix_data.extend_from_slice(governance.key.as_ref());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new_readonly(*governance.key, true),
                AccountMeta::new(*market.key, false),
            ],
            data: oracle_ix_data,
        },
        &[
            controller.clone(),
            governance.clone(),
            market.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )?;
    invoke_signed(
        &Instruction {
            program_id: *subledger_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new_readonly(*pool.key, false),
                AccountMeta::new(*market.key, false),
                AccountMeta::new_readonly(*percolator_program.key, false),
            ],
            data: vec![SUBLEDGER_IX_ACCEPT_OPERATOR],
        },
        &[
            controller.clone(),
            pool.clone(),
            market.clone(),
            percolator_program.clone(),
            subledger_program.clone(),
        ],
        &[&seeds],
    )
}

// accept_market_authority accounts:
// [governance, current_authority(signer), controller_pda, market(w), percolator_program]
// A permissionless creator can hand a market to the governance-bound controller;
// governance does not need to participate in or approve the donation. Percolator's
// market-authority update also rotates asset-0 roles that equal the outgoing key, but
// it does not migrate secondary asset_admin roles. Accept only an asset-0-only market
// or one whose secondary slots are all fully retired, with direct permissionless asset
// append disabled; controller-governed secondary assets can then be activated after
// the handoff. If the outgoing key owns funded
// asset-0 insurance or is the recorded backing provider, restore only those same roles
// so donating lifecycle control cannot donate segregated capital too. A later
// genesis-pool grant requires the external insurance balance to exit first.
fn process_accept_market_authority<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let governance = next_account_info(iter)?;
    let current = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    if !current.is_signer || iter.next().is_some() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    let (restore_insurance_authority, restore_insurance_operator, restore_outgoing_backing) = {
        let market_data = market.try_borrow_data()?;
        if !percolator_accounting::all_secondary_assets_retired(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            || percolator_accounting::read_permissionless_market_init_fee(&market_data)
                .map_err(|_| ProgramError::InvalidAccountData)?
                != 0
        {
            return Err(ProgramError::InvalidAccountData);
        }
        let current_bytes = current.key.to_bytes();
        let has_insurance = percolator_accounting::read_asset_insurance_remaining(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != 0;
        let insurance_authority =
            percolator_accounting::read_asset_insurance_authority(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let insurance_operator =
            percolator_accounting::read_asset_insurance_operator(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?;
        let restore_insurance_authority = has_insurance && insurance_authority == current_bytes;
        let restore_insurance_operator = has_insurance && insurance_operator == current_bytes;
        let asset_admin = percolator_accounting::read_asset_admin(&market_data, 0)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        // UpdateAuthority migrates asset_admin only while it still equals the outgoing
        // market authority. Restoring an outgoing funded value role while leaving a
        // separately delegated admin would make terminal recovery depend on that signer.
        if (restore_insurance_authority || restore_insurance_operator)
            && asset_admin != current_bytes
            && asset_admin != controller.key.to_bytes()
        {
            return Err(ProgramError::InvalidAccountData);
        }
        (
            restore_insurance_authority,
            restore_insurance_operator,
            percolator_accounting::read_asset_backing_authority(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?
                == current_bytes,
        )
    };
    let mut ix_data = vec![PERC_IX_UPDATE_AUTHORITY];
    ix_data.extend_from_slice(controller.key.as_ref());
    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*current.key, true),
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
            ],
            data: ix_data,
        },
        &[
            current.clone(),
            controller.clone(),
            market.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )?;

    for (kind, restore) in [
        (ASSET_AUTH_INSURANCE, restore_insurance_authority),
        (ASSET_AUTH_INSURANCE_OPERATOR, restore_insurance_operator),
        (ASSET_AUTH_BACKING_BUCKET, restore_outgoing_backing),
    ] {
        if !restore {
            continue;
        }
        let mut restore_data = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
        restore_data.extend_from_slice(&0u16.to_le_bytes());
        restore_data.push(kind);
        restore_data.extend_from_slice(current.key.as_ref());
        invoke_signed(
            &Instruction {
                program_id: *percolator_program.key,
                accounts: vec![
                    AccountMeta::new_readonly(*controller.key, true),
                    AccountMeta::new_readonly(*current.key, true),
                    AccountMeta::new(*market.key, false),
                ],
                data: restore_data,
            },
            &[
                controller.clone(),
                current.clone(),
                market.clone(),
                percolator_program.clone(),
            ],
            &[&seeds],
        )?;
    }
    Ok(())
}

// donate_insurance accounts:
// [donor(s), governance, controller_pda, market(w), donor_source(w),
//  controller_holding(w), percolator_vault(w), percolator_program, token_program]
// data: amount (u64)
//
// Permissionless and inbound-only. This lets a new market receive bootstrap
// surplus before custody moves to the genesis pool without exposing TopUpInsurance
// through the generic governance proxy.
fn process_donate_insurance<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if data.len() != 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let amount = u64::from_le_bytes(data.try_into().unwrap());
    if amount == 0 {
        return Err(ProgramError::InvalidArgument);
    }
    let iter = &mut accounts.iter();
    let donor = next_account_info(iter)?;
    let governance = next_account_info(iter)?;
    let controller = next_account_info(iter)?;
    let market = next_account_info(iter)?;
    let donor_source = next_account_info(iter)?;
    let controller_holding = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    if !donor.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let bump = controller_bump(
        program_id,
        governance,
        controller,
        market,
        percolator_program,
    )?;
    {
        let market_data = market.try_borrow_data()?;
        let controller_key = controller.key.to_bytes();
        if percolator_accounting::read_market_authority(&market_data)
            .map_err(|_| ProgramError::InvalidAccountData)?
            != controller_key
            || percolator_accounting::read_asset_insurance_authority(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?
                != controller_key
            || percolator_accounting::read_asset_insurance_operator(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?
                != controller_key
            || percolator_accounting::read_asset_admin(&market_data, 0)
                .map_err(|_| ProgramError::InvalidAccountData)?
                != controller_key
        {
            return Err(ProgramError::InvalidAccountData);
        }
    }
    let source = spl_token::state::Account::unpack(&donor_source.try_borrow_data()?)?;
    let holding = spl_token::state::Account::unpack(&controller_holding.try_borrow_data()?)?;
    if source.owner != *donor.key
        || holding.owner != *controller.key
        || source.mint != holding.mint
        || source.amount < amount
    {
        return Err(ProgramError::InvalidAccountData);
    }

    invoke(
        &spl_token::instruction::transfer(
            token_program.key,
            donor_source.key,
            controller_holding.key,
            donor.key,
            &[],
            amount,
        )?,
        &[
            donor_source.clone(),
            controller_holding.clone(),
            donor.clone(),
            token_program.clone(),
        ],
    )?;

    let mut ix_data = vec![PERC_IX_TOP_UP_INSURANCE];
    ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
    let bump_seed = [bump];
    let seeds = signer_seeds(
        governance.key,
        market.key,
        percolator_program.key,
        &bump_seed,
    );
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*controller.key, true),
                AccountMeta::new(*market.key, false),
                AccountMeta::new(*controller_holding.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: ix_data,
        },
        &[
            controller.clone(),
            market.clone(),
            controller_holding.clone(),
            percolator_vault.clone(),
            token_program.clone(),
            percolator_program.clone(),
        ],
        &[&seeds],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_denies_live_value_and_every_key_mutation_path() {
        let allowed = [19u8, 34, 35, 37, 38, 40, 49, 51, 55, 58, 62, 69];
        for tag in 0u8..=69 {
            assert_eq!(admin_tag_allowed(tag), allowed.contains(&tag), "tag {tag}");
        }
        for forbidden in [3u8, 4, 9, 13, 24, 32, 41, 50, 52, 56, 57, 59, 60, 61, 65] {
            assert!(!admin_tag_allowed(forbidden));
        }
    }

    fn asset_lifecycle_data(
        action: u8,
        authority: Pubkey,
        operator: Pubkey,
    ) -> alloc::vec::Vec<u8> {
        let mut data = vec![PERC_IX_UPDATE_ASSET_LIFECYCLE, action];
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&100u64.to_le_bytes());
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.extend_from_slice(authority.as_ref());
        data.extend_from_slice(operator.as_ref());
        data.extend_from_slice(Pubkey::new_unique().as_ref());
        data.extend_from_slice(Pubkey::new_unique().as_ref());
        assert_eq!(data.len(), UPDATE_ASSET_LIFECYCLE_LEN);
        data
    }

    #[test]
    fn activation_cannot_split_deposit_and_withdrawal_authorities() {
        let provider = Pubkey::new_unique();
        let safe = asset_lifecycle_data(ASSET_ACTION_ACTIVATE, provider, provider);
        assert_eq!(validate_admin_instruction_data(&safe), Ok(()));

        let split = asset_lifecycle_data(ASSET_ACTION_ACTIVATE, provider, Pubkey::new_unique());
        assert_eq!(
            validate_admin_instruction_data(&split),
            Err(ProgramError::InvalidInstructionData)
        );

        let mut truncated = safe;
        truncated.pop();
        assert_eq!(
            validate_admin_instruction_data(&truncated),
            Err(ProgramError::InvalidInstructionData)
        );

        let lifecycle_transition = asset_lifecycle_data(1, provider, Pubkey::new_unique());
        assert_eq!(
            validate_admin_instruction_data(&lifecycle_transition),
            Ok(())
        );
    }

    #[test]
    fn controller_pda_binds_governance_market_and_program() {
        let governance = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let percolator = Pubkey::new_unique();
        let (controller, _) = controller_address(&governance, &market, &percolator);
        assert_ne!(
            controller,
            controller_address(&Pubkey::new_unique(), &market, &percolator).0
        );
        assert_ne!(
            controller,
            controller_address(&governance, &Pubkey::new_unique(), &percolator).0
        );
        assert_ne!(
            controller,
            controller_address(&governance, &market, &Pubkey::new_unique()).0
        );
    }
}
