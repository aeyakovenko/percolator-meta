//! Market-0 TWAP buy/burn program — the percolator-facing link of the genesis
//! authority chain:
//!
//!   DAO (metadao_futarchy)  →  Squads multisig (1-week timelock)  →  THIS program
//!       →  percolator market-0 insurance
//!
//! After the genesis mint, the percolator market-0 insurance authority/operator is
//! rotated from the subledger to this program's `twap_authority` PDA. From then on
//! the TWAP is what touches insurance: it pulls the configured surplus share and
//! (in later slices) buys + burns COIN with it. The TWAP itself is *configured* only
//! by its `squads` controller — a Squads multisig whose `config_authority` is the
//! DAO — so the DAO controls percolator insurance only through the timelocked Squads
//! path. The pull crank is permissionless (anyone may turn it) but bounded by the
//! Squads-set parameters.
//!
//! This slice wires the on-chain keystone: the config that pins the whole chain, and
//! the `twap_authority` PDA signing the percolator insurance CPI. The Squads
//! vault-execute reconfigure path and the COIN buy/burn settlement build on top.
#![allow(clippy::result_large_err)]

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
    sysvar::Sysvar,
};

solana_program::declare_id!("TwapBuyBurn11111111111111111111111111111111");

// The Squads v4 program. The TWAP controller must be a multisig owned by it, so the
// configured controller is provably a real Squads multisig (whose config_authority
// is the DAO) and not an arbitrary key.
const SQUADS_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf");

// Squads v4 `Multisig` account discriminator (anchor account:Multisig). The
// config_authority is at bytes [40..72] of the account.
const SQUADS_MULTISIG_DISC: [u8; 8] = [224, 116, 121, 186, 68, 161, 79, 236];

// The twap_authority PDA seed — matches the `twap` lib's TWAP_AUTHORITY_SEED. The
// authority is bound to the Percolator program as well as the market so a
// permissionless config with a different CPI target cannot reuse the same signer.
const TWAP_AUTHORITY_SEED: &[u8] = b"market-0-twap";
const CONFIG_SEED: &[u8] = b"twap_config";

const CONFIG_DISC: [u8; 8] = *b"TWAPCFG1";
const CONFIG_SIZE: usize = 200;

// Default surplus share routed to buy/burn (the rest is retained as insurance).
const DEFAULT_SURPLUS_BUY_BURN_BPS: u16 = 8_000;
const BPS_DENOMINATOR: u16 = 10_000;

// Percolator CPI tags (verified against the real v16 program via the subledger).
const PERC_IX_WITHDRAW_INSURANCE_LIMITED: u8 = 23;
const PERC_IX_UPDATE_ASSET_AUTHORITY: u8 = 65;
const ASSET_AUTH_INSURANCE: u8 = 1; // insurance_authority (gates TopUpInsurance / deposits)
const ASSET_AUTH_INSURANCE_OPERATOR: u8 = 2;

const IX_INIT_CONFIG: u8 = 0;
const IX_PULL_SURPLUS: u8 = 1;
// Reconfigure the surplus buy/burn share. Gated on the Squads VAULT PDA, which can
// only sign via a multisig vault-transaction execute — i.e. after a DAO proposal
// clears the 1-week Squads timelock. This is the on-chain Squads -> TWAP control.
const IX_RECONFIGURE: u8 = 2;
// Accept the percolator asset-0 INSURANCE_OPERATOR role for the twap_authority PDA.
// This is the handoff: the squads vault (the current asset_admin) co-signs via a
// timelock'd execute, and the program co-signs as twap_authority (percolator's
// UpdateAssetAuthority requires the NEW authority to consent). After this the TWAP,
// not the subledger, is the insurance operator.
const IX_ACCEPT_OPERATOR: u8 = 3;
// Set the surplus floor (reserved depositor principal). Squads-vault-gated like
// reconfigure, so it only lands through a timelock'd DAO proposal. Lowering it below the
// live reserved principal is the dangerous move and is exactly what the timelock guards.
const IX_SET_RESERVED_FLOOR: u8 = 4;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

// The config PDA commits to ALL caller-supplied bindings, not just the market. Keying
// it on market alone made init_config (which is permissionless) front-run squattable:
// an attacker could stand up a throwaway Squads multisig (config_authority = itself),
// pass the internal consistency check, and init the per-market config first with their
// own bindings — permanently blocking the real DAO's deployment for that market (the
// squatted config is inert, but the PDA is taken and cannot be re-initialized). By
// folding squads_multisig + coin_mint + percolator_program into the seed, the only
// config that can exist at the legit address is one carrying the legit bindings (which
// in turn forces the real metadao_futarchy via the config_authority check) — so a
// front-run at that address merely reproduces the correct config and does no harm; any
// attacker variation lands at a different PDA the real deployment ignores. (finding P)
fn config_seeds<'a>(
    market: &'a Pubkey,
    squads_multisig: &'a Pubkey,
    coin_mint: &'a Pubkey,
    percolator_program: &'a Pubkey,
) -> [&'a [u8]; 5] {
    [
        CONFIG_SEED,
        market.as_ref(),
        squads_multisig.as_ref(),
        coin_mint.as_ref(),
        percolator_program.as_ref(),
    ]
}

fn authority_seeds<'a>(market: &'a Pubkey, percolator_program: &'a Pubkey) -> [&'a [u8]; 3] {
    [
        TWAP_AUTHORITY_SEED,
        market.as_ref(),
        percolator_program.as_ref(),
    ]
}

// The Squads multisig's default (index 0) vault PDA — the address that signs the
// inner instructions of an executed multisig vault-transaction.
fn squads_default_vault(multisig: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"multisig", multisig.as_ref(), b"vault", &[0u8]],
        &SQUADS_PROGRAM_ID,
    )
    .0
}

fn perc_vault_authority(market_slab: &Pubkey, percolator_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market_slab.as_ref()], percolator_program).0
}

// Byte offset of the asset-0 `insurance` u128 inside a percolator market slab. Solana
// account data is globally readable, so we read it straight from the slab bytes — no
// accessor API, no percolator linkage. The slab's zero-copy MarketGroupV16 header is a
// repr(C) Pod of `[u8;N]` newtypes (align 1, no padding) at MARKET_GROUP_OFF =
// HEADER_LEN(16)+WRAPPER_CONFIG_LEN(432)=448; `insurance` sits at +285 within it
// (market_group_id 32 + V16ConfigAccount 233 + asset_slot_capacity 4 + vault 16; it is the
// field right after `vault` and before `c_tot`). The
// `insurance_offset_matches_real_percolator_slab` canary pins this against the live binary.
const INSURANCE_OFFSET: usize = 448 + 285;

/// Read the market's asset-0 insurance balance directly from the slab account bytes.
fn read_asset0_insurance(slab_data: &[u8]) -> Result<u128, ProgramError> {
    let b = slab_data
        .get(INSURANCE_OFFSET..INSURANCE_OFFSET + 16)
        .ok_or(ProgramError::InvalidAccountData)?;
    Ok(u128::from_le_bytes(b.try_into().unwrap()))
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct Config {
    coin_mint: Pubkey,
    market_slab: Pubkey,
    percolator_program: Pubkey,
    /// The Squads multisig that controls (reconfigures/rotates) this TWAP. Its
    /// `config_authority` is the DAO, so the DAO governs the TWAP only via Squads.
    squads_multisig: Pubkey,
    /// The winning genesis DAO (metadao futarchy authority).
    metadao_futarchy: Pubkey,
    surplus_buy_burn_bps: u16,
    market_0_domain: u8,
    config_bump: u8,
    authority_bump: u8,
    /// The asset-0 insurance amount pull_surplus must NEVER pull below — the reserved
    /// depositor principal (+ any retained buffer). pull_surplus may move at most
    /// `insurance - reserved_floor`. Initialized to u128::MAX (no pulls) and lowered only
    /// by the DAO through a timelock'd Squads `set_reserved_floor`, so a permissionless
    /// crank can never reach principal (closes finding O).
    reserved_floor: u128,
}

impl Config {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < CONFIG_SIZE || data[..8] != CONFIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            market_slab: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            percolator_program: Pubkey::new_from_array(data[72..104].try_into().unwrap()),
            squads_multisig: Pubkey::new_from_array(data[104..136].try_into().unwrap()),
            metadao_futarchy: Pubkey::new_from_array(data[136..168].try_into().unwrap()),
            surplus_buy_burn_bps: u16::from_le_bytes(data[168..170].try_into().unwrap()),
            market_0_domain: data[170],
            config_bump: data[171],
            authority_bump: data[172],
            reserved_floor: u128::from_le_bytes(data[173..189].try_into().unwrap()),
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&CONFIG_DISC);
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[40..72].copy_from_slice(self.market_slab.as_ref());
        data[72..104].copy_from_slice(self.percolator_program.as_ref());
        data[104..136].copy_from_slice(self.squads_multisig.as_ref());
        data[136..168].copy_from_slice(self.metadao_futarchy.as_ref());
        data[168..170].copy_from_slice(&self.surplus_buy_burn_bps.to_le_bytes());
        data[170] = self.market_0_domain;
        data[171] = self.config_bump;
        data[172] = self.authority_bump;
        data[173..189].copy_from_slice(&self.reserved_floor.to_le_bytes());
        data[189..CONFIG_SIZE].fill(0);
    }
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
        IX_INIT_CONFIG => process_init_config(program_id, accounts, data),
        IX_PULL_SURPLUS => process_pull_surplus(program_id, accounts, data),
        IX_RECONFIGURE => process_reconfigure(program_id, accounts, data),
        IX_SET_RESERVED_FLOOR => process_set_reserved_floor(program_id, accounts, data),
        IX_ACCEPT_OPERATOR => process_accept_operator(program_id, accounts, data),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn read_u64(data: &[u8]) -> Result<u64, ProgramError> {
    if data.len() != 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok(u64::from_le_bytes(data.try_into().unwrap()))
}

// init_config accounts: [payer(s,w), coin_mint, market_slab, config(pda,w),
//   squads_multisig, metadao_futarchy, percolator_program, system]
//
// Pins the whole authority chain: the controller must be a real Squads multisig and
// the DAO (metadao_futarchy) is recorded. The twap_authority PDA derived here is the
// address that must hold percolator's insurance authority/operator role.
fn process_init_config(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let squads_multisig = next_account_info(iter)?;
    let metadao_futarchy = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    // The controller must be a genuine Squads multisig — that is the only account
    // through which the DAO (its config_authority) can ever reach this program.
    if *squads_multisig.owner != SQUADS_PROGRAM_ID {
        return Err(ProgramError::IllegalOwner);
    }
    if *metadao_futarchy.key == Pubkey::default() || *percolator_program.key == Pubkey::default() {
        return Err(ProgramError::InvalidAccountData);
    }
    // ...and that multisig must actually be config-controlled by the named DAO. This
    // is the DAO->Squads link: without it, a TWAP could be wired to a Squads multisig
    // whose config_authority is an attacker (not the DAO), so the DAO would not in
    // fact govern this program. Read the multisig's config_authority (bytes [40..72]
    // of a Squads `Multisig`) and require it to equal metadao_futarchy.
    {
        let ms = squads_multisig.try_borrow_data()?;
        if ms.len() < 72 || ms[..8] != SQUADS_MULTISIG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let multisig_config_authority = Pubkey::new_from_array(ms[40..72].try_into().unwrap());
        if multisig_config_authority != *metadao_futarchy.key {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    let (expected_config, config_bump) = Pubkey::find_program_address(
        &config_seeds(
            market_slab.key,
            squads_multisig.key,
            coin_mint.key,
            percolator_program.key,
        ),
        program_id,
    );
    if *config_account.key != expected_config {
        return Err(ProgramError::InvalidSeeds);
    }
    if config_account.lamports() != 0 || config_account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let (_twap_authority, authority_bump) = Pubkey::find_program_address(
        &authority_seeds(market_slab.key, percolator_program.key),
        program_id,
    );

    let rent = solana_program::rent::Rent::get()?;
    let bump_arr = [config_bump];
    let seeds: [&[u8]; 6] = [
        CONFIG_SEED,
        market_slab.key.as_ref(),
        squads_multisig.key.as_ref(),
        coin_mint.key.as_ref(),
        percolator_program.key.as_ref(),
        &bump_arr,
    ];
    invoke_signed(
        &system_instruction::create_account(
            payer.key,
            config_account.key,
            rent.minimum_balance(CONFIG_SIZE),
            CONFIG_SIZE as u64,
            program_id,
        ),
        &[
            payer.clone(),
            config_account.clone(),
            system_program.clone(),
        ],
        &[&seeds],
    )?;

    let config = Config {
        coin_mint: *coin_mint.key,
        market_slab: *market_slab.key,
        percolator_program: *percolator_program.key,
        squads_multisig: *squads_multisig.key,
        metadao_futarchy: *metadao_futarchy.key,
        surplus_buy_burn_bps: DEFAULT_SURPLUS_BUY_BURN_BPS,
        market_0_domain: 0,
        config_bump,
        authority_bump,
        // No pulls until the DAO sets a real floor via timelock'd set_reserved_floor.
        reserved_floor: u128::MAX,
    };
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// pull_surplus accounts: [cranker(s), config, twap_authority(pda), market_slab(w),
//   holding(w, twap_authority-owned token acct), percolator_vault(w), vault_authority,
//   percolator_program, token_program]
// data: amount (u64)
//
// Permissionless crank: the twap_authority PDA (percolator insurance operator) signs
// WithdrawInsuranceLimited, pulling `amount` of insurance surplus into a holding
// account it owns. The COIN buy + burn settlement is a later slice. The TWAP can
// only ever move insurance because it holds the percolator operator role — granted by
// the rotation from the subledger, itself authorised through the Squads/DAO chain.
//
// SURPLUS-BOUNDED (finding O fixed): `amount` is capped to `insurance - reserved_floor`.
// The live asset-0 insurance is read straight from the slab bytes (read_asset0_insurance);
// reserved_floor is the reserved depositor principal, lowered only by the DAO via a
// timelock'd set_reserved_floor and initialized to u128::MAX. So a permissionless crank can
// never reach principal, regardless of percolator's policy mode.
fn process_pull_surplus(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let holding = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let vault_authority = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !cranker.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *token_program.key != spl_token::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    // Bind every account to the pinned config.
    if *market_slab.key != config.market_slab
        || *percolator_program.key != config.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // Re-derive the twap_authority so the signing seeds are trusted.
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 4] = [
        TWAP_AUTHORITY_SEED,
        config.market_slab.as_ref(),
        config.percolator_program.as_ref(),
        &auth_bump,
    ];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }
    if *vault_authority.key != perc_vault_authority(market_slab.key, percolator_program.key) {
        return Err(ProgramError::InvalidSeeds);
    }
    // Percolator requires the WithdrawInsuranceLimited destination to be owned by the
    // operator (the twap_authority). Holding must be a token account it owns.
    let holding_state = spl_token::state::Account::unpack(&holding.try_borrow_data()?)?;
    if holding_state.owner != expected_authority {
        return Err(ProgramError::InvalidAccountData);
    }

    // SURPLUS FLOOR (finding O): pull at most `insurance - reserved_floor`. The live asset-0
    // insurance is read straight from the slab bytes (Solana account data is globally
    // readable — no accessor API needed). reserved_floor is the reserved depositor principal,
    // set only by the DAO through a timelock'd set_reserved_floor and initialized to
    // u128::MAX, so a permissionless crank can never reach principal.
    let insurance = read_asset0_insurance(&market_slab.try_borrow_data()?)?;
    let surplus = insurance.saturating_sub(config.reserved_floor);
    if (amount as u128) > surplus {
        return Err(ProgramError::InsufficientFunds);
    }

    let mut ix_data = vec![PERC_IX_WITHDRAW_INSURANCE_LIMITED];
    ix_data.extend_from_slice(&(amount as u128).to_le_bytes());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*twap_authority.key, true),
                AccountMeta::new(*market_slab.key, false),
                AccountMeta::new(*holding.key, false),
                AccountMeta::new(*percolator_vault.key, false),
                AccountMeta::new_readonly(*vault_authority.key, false),
                AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: ix_data,
        },
        &[
            twap_authority.clone(),
            market_slab.clone(),
            holding.clone(),
            percolator_vault.clone(),
            vault_authority.clone(),
            token_program.clone(),
            percolator_program.clone(),
        ],
        &[&auth_seeds],
    )?;
    Ok(())
}

// reconfigure accounts: [squads_vault(signer), config(w)]
// data: new_surplus_buy_burn_bps (u16)
//
// Squads -> TWAP control: only the config's Squads multisig default vault PDA may
// reconfigure, and that PDA can only sign as the executor of a multisig
// vault-transaction — which requires a DAO proposal to clear the 1-week timelock.
fn process_reconfigure(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;

    if data.len() != 2 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let new_bps = u16::from_le_bytes(data.try_into().unwrap());
    if new_bps == 0 || new_bps > BPS_DENOMINATOR {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !squads_vault.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    // The signer must be the default vault PDA of the configured Squads multisig.
    if *squads_vault.key != squads_default_vault(&config.squads_multisig) {
        return Err(ProgramError::IllegalOwner);
    }
    config.surplus_buy_burn_bps = new_bps;
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// set_reserved_floor accounts: [squads_vault(signer), config(w)]
// data: new_reserved_floor (u128)
//
// Squads -> TWAP control (finding O): set the surplus floor — the asset-0 insurance amount
// pull_surplus must never pull below (the reserved depositor principal). Only the config's
// Squads vault may call it, and only as the executor of a timelock'd vault-transaction, so
// lowering the floor (the dangerous direction — it exposes more insurance to the
// permissionless crank) is delayed a full week in the clear.
fn process_set_reserved_floor(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;

    if data.len() != 16 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let new_floor = u128::from_le_bytes(data.try_into().unwrap());
    if !squads_vault.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if *squads_vault.key != squads_default_vault(&config.squads_multisig) {
        return Err(ProgramError::IllegalOwner);
    }
    config.reserved_floor = new_floor;
    config.serialize(&mut config_account.try_borrow_mut_data()?);
    Ok(())
}

// accept_operator accounts: [squads_vault(signer), config, twap_authority(pda),
//   market_slab(w), percolator_program]
//
// The handoff. Gated on the config's Squads vault (the percolator asset-0
// asset_admin) — reachable only via a timelock'd multisig execute. The program
// co-signs as twap_authority (percolator requires the incoming authority to consent),
// rotating the asset-0 INSURANCE_OPERATOR from the subledger to the twap_authority.
//
// After this, pull_surplus (permissionless) is the operator's only insurance path, and it
// is surplus-floor-bounded (finding O fixed): it pulls at most `insurance - reserved_floor`.
// The DAO proposal that performs the handoff should also set the reserved_floor (to the
// reserved depositor principal) via set_reserved_floor and rotate the policy to surplus-mode
// — until reserved_floor is set it is u128::MAX, so no surplus can be pulled at all.
fn process_accept_operator(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let squads_vault = next_account_info(iter)?;
    let config_account = next_account_info(iter)?;
    let twap_authority = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !squads_vault.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if config_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let config = Config::deserialize(&config_account.try_borrow_data()?)?;
    if *squads_vault.key != squads_default_vault(&config.squads_multisig) {
        return Err(ProgramError::IllegalOwner);
    }
    if *market_slab.key != config.market_slab
        || *percolator_program.key != config.percolator_program
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let auth_bump = [config.authority_bump];
    let auth_seeds: [&[u8]; 4] = [
        TWAP_AUTHORITY_SEED,
        config.market_slab.as_ref(),
        config.percolator_program.as_ref(),
        &auth_bump,
    ];
    let expected_authority = Pubkey::create_program_address(&auth_seeds, program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if *twap_authority.key != expected_authority {
        return Err(ProgramError::InvalidSeeds);
    }

    // UpdateAssetAuthority(asset 0, INSURANCE_OPERATOR, new = twap_authority).
    // Signers: squads_vault (current asset_admin, propagated from the execute) and
    // twap_authority (the consenting new authority, via invoke_signed seeds).
    let mut ix_data = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
    ix_data.extend_from_slice(&0u16.to_le_bytes()); // asset_index 0
    ix_data.push(ASSET_AUTH_INSURANCE_OPERATOR);
    ix_data.extend_from_slice(twap_authority.key.as_ref());
    invoke_signed(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*squads_vault.key, true),
                AccountMeta::new_readonly(*twap_authority.key, true),
                AccountMeta::new(*market_slab.key, false),
            ],
            data: ix_data,
        },
        &[
            squads_vault.clone(),
            twap_authority.clone(),
            market_slab.clone(),
            percolator_program.clone(),
        ],
        &[&auth_seeds],
    )?;

    // Finding S: atomically rotate the asset-0 insurance AUTHORITY (kind 1, which gates
    // TopUpInsurance / deposits) away from the subledger pool to the Squads vault. Otherwise
    // the pool keeps kind 1 and subledger deposits still work AFTER the handoff — and since
    // the surplus floor is a static snapshot, such a post-handoff deposit raises insurance
    // above the floor and a permissionless cranker drains its principal as "surplus" (LOF).
    // Both `current` and `new` are the Squads vault (the asset_admin), which co-signs here
    // (propagated from the timelock'd execute), so no extra consent is needed. After this no
    // one can deposit into market-0 insurance, so the static floor is sound.
    let mut auth_ix = vec![PERC_IX_UPDATE_ASSET_AUTHORITY];
    auth_ix.extend_from_slice(&0u16.to_le_bytes()); // asset_index 0
    auth_ix.push(ASSET_AUTH_INSURANCE);
    auth_ix.extend_from_slice(squads_vault.key.as_ref()); // new = the Squads vault
    invoke(
        &Instruction {
            program_id: *percolator_program.key,
            accounts: vec![
                AccountMeta::new_readonly(*squads_vault.key, true), // current asset_admin
                AccountMeta::new_readonly(*squads_vault.key, true), // new (co-signs, same key)
                AccountMeta::new(*market_slab.key, false),
            ],
            data: auth_ix,
        },
        &[squads_vault.clone(), market_slab.clone(), percolator_program.clone()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips() {
        let c = Config {
            coin_mint: Pubkey::new_unique(),
            market_slab: Pubkey::new_unique(),
            percolator_program: Pubkey::new_unique(),
            squads_multisig: Pubkey::new_unique(),
            metadao_futarchy: Pubkey::new_unique(),
            surplus_buy_burn_bps: DEFAULT_SURPLUS_BUY_BURN_BPS,
            market_0_domain: 0,
            config_bump: 254,
            authority_bump: 251,
            reserved_floor: 123_456_789,
        };
        let mut buf = [0u8; CONFIG_SIZE];
        c.serialize(&mut buf);
        let d = Config::deserialize(&buf).unwrap();
        assert_eq!(d.coin_mint, c.coin_mint);
        assert_eq!(d.market_slab, c.market_slab);
        assert_eq!(d.squads_multisig, c.squads_multisig);
        assert_eq!(d.metadao_futarchy, c.metadao_futarchy);
        assert_eq!(d.surplus_buy_burn_bps, 8_000);
        assert_eq!(d.authority_bump, 251);
        assert_eq!(d.reserved_floor, 123_456_789);
        assert!(d.surplus_buy_burn_bps < BPS_DENOMINATOR);
    }

    #[test]
    fn twap_authority_is_bound_to_percolator_program() {
        let market = Pubkey::new_unique();
        let real_percolator = Pubkey::new_unique();
        let fake_percolator = Pubkey::new_unique();

        let (real_authority, _) =
            Pubkey::find_program_address(&authority_seeds(&market, &real_percolator), &id());
        let (fake_authority, _) =
            Pubkey::find_program_address(&authority_seeds(&market, &fake_percolator), &id());

        assert_ne!(
            real_authority, fake_authority,
            "a config with a different Percolator program must not reuse the same TWAP signer"
        );
    }
}
