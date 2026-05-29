//! Insurance deposit program: users deposit collateral into per-market vaults
//! and earn COIN (DAO token) as yield. No lockup — withdraw anytime.
//! Non-upgradeable. No admin keys. CoinConfig authority gates bootstrap/live
//! phase transitions and market registration.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

#[allow(unused_imports)]
use alloc::format; // Required by entrypoint! macro in SBF builds

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    declare_id, entrypoint,
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

use governance_adapter::{
    authority_address as governance_authority_address, id as governance_program_id,
};

mod percolator_abi {
    use solana_program::{declare_id, program_error::ProgramError};

    declare_id!("Perco1ator111111111111111111111111111111111");

    const MAGIC: u64 = 0x5045_5243_5631_3600; // "PERCV16\0"
    const VERSION: u16 = 16;
    const KIND_MARKET: u8 = 1;
    const HEADER_LEN: usize = 16;
    const WRAPPER_CONFIG_LEN: usize = 624;
    const CFG_ADMIN_OFF: usize = HEADER_LEN;
    const CFG_COLLATERAL_MINT_OFF: usize = HEADER_LEN + 32;
    const CFG_SECONDARY_COLLATERAL_MINT_OFF: usize = HEADER_LEN + 64;
    const CFG_INSURANCE_AUTHORITY_OFF: usize = HEADER_LEN + 192;
    const CFG_INSURANCE_OPERATOR_OFF: usize = HEADER_LEN + 224;
    const CFG_BACKING_BUCKET_AUTHORITY_OFF: usize = HEADER_LEN + 256;

    pub struct MarketConfig {
        pub admin: [u8; 32],
        pub collateral_mint: [u8; 32],
        pub secondary_collateral_mint: [u8; 32],
        pub insurance_authority: [u8; 32],
        pub insurance_operator: [u8; 32],
        pub backing_bucket_authority: [u8; 32],
    }

    fn read_u16(data: &[u8], off: usize) -> Result<u16, ProgramError> {
        let bytes = data
            .get(off..off + 2)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u64(data: &[u8], off: usize) -> Result<u64, ProgramError> {
        let bytes = data
            .get(off..off + 8)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_pubkey_bytes(data: &[u8], off: usize) -> Result<[u8; 32], ProgramError> {
        let mut out = [0u8; 32];
        out.copy_from_slice(
            data.get(off..off + 32)
                .ok_or(ProgramError::InvalidAccountData)?,
        );
        Ok(out)
    }

    fn check_header(data: &[u8], kind: u8, min_len: usize) -> Result<(), ProgramError> {
        if data.len() < min_len {
            return Err(ProgramError::InvalidAccountData);
        }
        if read_u64(data, 0)? != MAGIC || read_u16(data, 8)? != VERSION || data[10] != kind {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(())
    }

    pub fn read_market_config(data: &[u8]) -> Result<MarketConfig, ProgramError> {
        check_header(data, KIND_MARKET, HEADER_LEN + WRAPPER_CONFIG_LEN)?;

        let config = MarketConfig {
            admin: read_pubkey_bytes(data, CFG_ADMIN_OFF)?,
            collateral_mint: read_pubkey_bytes(data, CFG_COLLATERAL_MINT_OFF)?,
            secondary_collateral_mint: read_pubkey_bytes(data, CFG_SECONDARY_COLLATERAL_MINT_OFF)?,
            insurance_authority: read_pubkey_bytes(data, CFG_INSURANCE_AUTHORITY_OFF)?,
            insurance_operator: read_pubkey_bytes(data, CFG_INSURANCE_OPERATOR_OFF)?,
            backing_bucket_authority: read_pubkey_bytes(data, CFG_BACKING_BUCKET_AUTHORITY_OFF)?,
        };
        if config.collateral_mint == [0u8; 32]
            || (config.secondary_collateral_mint != [0u8; 32]
                && config.secondary_collateral_mint == config.collateral_mint)
        {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(config)
    }
}

/// Instruction tags
const IX_INIT_COIN_CONFIG: u8 = 3;
const IX_MINT_REWARD: u8 = 8;
const IX_TRANSFER_MINT_AUTHORITY: u8 = 10;
const IX_ACTIVATE_LIVE: u8 = 11;
const IX_INIT_PERCOLATOR_MARKET: u8 = 19;
const IX_PERCOLATOR_ADMIN: u8 = 20;
const IX_INIT_GENESIS_BOOTSTRAP: u8 = 21;
const IX_GENESIS_DEPOSIT: u8 = 22;
const IX_GENESIS_WITHDRAW: u8 = 23;
const IX_TRIGGER_GENESIS_DISTRIBUTION: u8 = 24;
const IX_FINALIZE_GENESIS: u8 = 25;
const IX_DRAW_GENESIS_SURPLUS: u8 = 26;
const IX_KICKSTART_GENESIS_MARKET: u8 = 27;
const IX_RECOVER_GENESIS_MARKET: u8 = 28;
const IX_INIT_GENESIS_DISTRIBUTION: u8 = 29;
const IX_VOTE_GENESIS_DISTRIBUTION: u8 = 30;
const IX_APPROVE_BUILDER: u8 = 31;
const IX_INIT_GENESIS_SQUADS: u8 = 32;
const IX_HANDOVER_GENESIS_SQUADS: u8 = 33;

/// Percolator instruction tags we CPI into
const PERC_IX_INIT_MARKET: u8 = 0;
const PERC_IX_CLOSE_SLAB: u8 = 13;
const PERC_IX_RESOLVE_MARKET: u8 = 19;
// Referenced only by the `futarchy_admin_proxy_is_lifecycle_scoped` unit test
// (asserts UpdateAuthority is NOT in the admin-proxy allow-list).
#[allow(dead_code)]
const PERC_IX_UPDATE_AUTHORITY: u8 = 32;
const PERC_IX_UPDATE_INSURANCE_POLICY: u8 = 33;
const PERC_IX_CONFIGURE_HYBRID_ORACLE: u8 = 34;
const PERC_IX_CONFIGURE_EWMA_MARK: u8 = 35;
const PERC_IX_UPDATE_LIQUIDATION_FEE_POLICY: u8 = 37;
const PERC_IX_CONFIGURE_PERMISSIONLESS_RESOLVE: u8 = 38;
const PERC_IX_UPDATE_ASSET_LIFECYCLE: u8 = 40;
const PERC_IX_WITHDRAW_INSURANCE: u8 = 41;
const PERC_IX_UPDATE_MAINTENANCE_FEE_POLICY: u8 = 49;
const PERC_IX_TOP_UP_INSURANCE: u8 = 9;
const PERC_IX_TOP_UP_BACKING_BUCKET: u8 = 24;
const PERC_IX_WITHDRAW_INSURANCE_LIMITED: u8 = 23;
const PERC_IX_WITHDRAW_BACKING_BUCKET: u8 = 50;
const PERC_IX_WITHDRAW_BACKING_BUCKET_EARNINGS: u8 = 52;
const PERC_IX_UPDATE_BACKING_FEE_POLICY: u8 = 51;
const PERC_IX_UPDATE_TRADE_FEE_POLICY: u8 = 55;
const PERC_IX_UPDATE_FEE_REDIRECT_POLICY: u8 = 58;
const PERC_IX_UPDATE_MARKET_INIT_FEE_POLICY: u8 = 59;
const PERC_IX_UPDATE_BASE_UNIT_MINTS: u8 = 60;
const PERC_IX_CONFIGURE_AUTH_MARK: u8 = 62;
const PERC_IX_WITHDRAW_INSURANCE_DOMAIN: u8 = 57;

/// Genesis market insurance withdraw policy: 100% of deposited principal is
/// recoverable (deposits_only caps to principal, never market profits).
const GENESIS_INSURANCE_WITHDRAW_MAX_BPS: u16 = 10_000;

const GENESIS_RECOVER_INSURANCE_LIMITED: u8 = 0;
const GENESIS_RECOVER_BACKING: u8 = 1;
const GENESIS_RECOVER_BACKING_EARNINGS: u8 = 2;
const GENESIS_RECOVER_INSURANCE_TERMINAL: u8 = 3;
const GENESIS_RECOVER_INSURANCE_DOMAIN: u8 = 4;

// ============================================================================
// Account sizes
// ============================================================================

/// CoinConfig: 8 + 32 + 8 + 8 + 8 + 1 + 7 = 72
const COIN_CFG_SIZE: usize = 8 + 32 + 8 + 8 + 8 + 1 + 7;
/// GenesisConfig: base-token bootstrap deposits, fixed supply, and the running
/// global vote tallies (total voted principal for quorum, total cast log-weight
/// for the 50%-of-weight winner test).
const GENESIS_CFG_SIZE: usize = 160;
/// GenesisPosition: per-user base-unit deposit plus this voter's single backed
/// proposal and the weight/principal it currently contributes.
const GENESIS_POSITION_SIZE: usize = 112;
/// GenesisDistribution: a candidate full-supply distribution and its support.
const GENESIS_DISTRIBUTION_SIZE: usize = 120;
/// BuilderApproval: governed registry entry for approved builder code.
const BUILDER_APPROVAL_SIZE: usize = 152;

// Discriminators
const COIN_CFG_DISC: [u8; 8] = *b"CCFGV002";
const GENESIS_CFG_DISC: [u8; 8] = *b"GENCFG01";
const GENESIS_POSITION_DISC: [u8; 8] = *b"GENPOS01";
const GENESIS_DISTRIBUTION_DISC: [u8; 8] = *b"GENDIST1";
const BUILDER_APPROVAL_DISC: [u8; 8] = *b"BLDAPP01";

const PHASE_BOOTSTRAP: u8 = 0;
const PHASE_LIVE: u8 = 1;

/// Default genesis deposit window: roughly one week at 400ms slots.

// ============================================================================
// PDA seeds
// ============================================================================

fn mint_authority_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_mint_authority", coin_mint.as_ref()]
}

fn coin_cfg_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"coin_cfg", coin_mint.as_ref()]
}

fn market_admin_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"percolator_market_admin", coin_mint.as_ref()]
}

fn genesis_cfg_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"genesis_cfg", coin_mint.as_ref()]
}

fn genesis_vault_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
    [b"genesis_vault", coin_mint.as_ref()]
}

fn genesis_position_seeds<'a>(genesis_cfg: &'a Pubkey, user: &'a Pubkey) -> [&'a [u8]; 3] {
    [b"genesis_position", genesis_cfg.as_ref(), user.as_ref()]
}

fn genesis_distribution_seeds<'a>(
    genesis_cfg: &'a Pubkey,
    proposal_id: &'a [u8; 8],
) -> [&'a [u8]; 3] {
    [b"genesis_distribution", genesis_cfg.as_ref(), proposal_id]
}

// ============================================================================
// Squads v4 handover
// ============================================================================
// The genesis market is born under a program-owned Squads v4 multisig: a
// controlled 1/1 multisig with a 48h timelock whose `config_authority` is held
// by this program's `market_admin` PDA during bootstrap. Control transfers to
// the winning genesis DAO by rotating that `config_authority` — percolator's own
// `UpdateAuthority` is never touched (no incoming-authority consent needed).
//
// CPI encodings are hand-rolled (no Anchor dep). See tests/squads_handover.rs
// for the standalone proofs these handlers mirror.
mod squads {
    use super::Pubkey;

    /// Squads v4 program (mainnet).
    pub const PROGRAM_ID: Pubkey =
        solana_program::pubkey!("SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf");

    // Anchor discriminators, sha256("global:<ix>")[..8].
    pub const IX_MULTISIG_CREATE_V2: [u8; 8] = [50, 221, 199, 93, 40, 245, 139, 233];
    pub const IX_SET_CONFIG_AUTHORITY: [u8; 8] = [143, 93, 199, 143, 92, 169, 193, 232];

    pub const SEED_PREFIX: &[u8] = b"multisig";
    pub const SEED_MULTISIG: &[u8] = b"multisig";

    /// Permission bitmask: Initiate | Vote | Execute.
    pub const PERM_ALL: u8 = 7;
    /// 48-hour timelock, in seconds.
    pub const TIMELOCK_48H_SECS: u32 = 48 * 60 * 60;

    /// Seeds for this program's per-coin Squads create-key PDA. The create-key
    /// makes the multisig address deterministic from `coin_mint`.
    pub fn create_key_seeds(coin_mint: &Pubkey) -> [&[u8]; 2] {
        [b"genesis_squads", coin_mint.as_ref()]
    }

    /// Derive the Squads multisig address for a given create-key.
    pub fn multisig_address(create_key: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[SEED_PREFIX, SEED_MULTISIG, create_key.as_ref()],
            &PROGRAM_ID,
        )
        .0
    }
}

fn builder_approval_seeds<'a>(
    coin_mint: &'a Pubkey,
    builder_program: &'a Pubkey,
    code_hash: &'a [u8; 32],
) -> [&'a [u8]; 4] {
    [
        b"builder_approval",
        coin_mint.as_ref(),
        builder_program.as_ref(),
        code_hash,
    ]
}

// ============================================================================
// Instruction deserialization
// ============================================================================

fn read_u8(data: &mut &[u8]) -> Result<u8, ProgramError> {
    if data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let val = data[0];
    *data = &data[1..];
    Ok(val)
}

fn read_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let val = u64::from_le_bytes(data[..8].try_into().unwrap());
    *data = &data[8..];
    Ok(val)
}

fn read_optional_u64(data: &mut &[u8]) -> Result<u64, ProgramError> {
    if data.is_empty() {
        return Ok(0);
    }
    let value = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
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

// ============================================================================
// CoinConfig — shared across all markets using the same COIN mint
// ============================================================================

struct CoinConfig {
    authority: Pubkey,
    bootstrap_start_slot: u64,
    bootstrap_delay_slots: u64,
    live_slot: u64,
    phase: u8,
}

impl CoinConfig {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < COIN_CFG_SIZE {
            return Err(ProgramError::InvalidAccountData);
        }
        if data[..8] != COIN_CFG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let authority = Pubkey::new_from_array(data[8..40].try_into().unwrap());
        let bootstrap_start_slot = u64::from_le_bytes(data[40..48].try_into().unwrap());
        let bootstrap_delay_slots = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let live_slot = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let phase = data[64];
        match phase {
            PHASE_BOOTSTRAP | PHASE_LIVE => {}
            _ => return Err(ProgramError::InvalidAccountData),
        }
        Ok(Self {
            authority,
            bootstrap_start_slot,
            bootstrap_delay_slots,
            live_slot,
            phase,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&COIN_CFG_DISC);
        data[8..40].copy_from_slice(self.authority.as_ref());
        data[40..48].copy_from_slice(&self.bootstrap_start_slot.to_le_bytes());
        data[48..56].copy_from_slice(&self.bootstrap_delay_slots.to_le_bytes());
        data[56..64].copy_from_slice(&self.live_slot.to_le_bytes());
        data[64] = self.phase;
        data[65..COIN_CFG_SIZE].fill(0);
    }

    fn is_live(&self) -> bool {
        self.phase == PHASE_LIVE
    }
}

// ============================================================================
// GenesisConfig / GenesisPosition — bootstrap vote and principal ledger
// ============================================================================

struct GenesisConfig {
    coin_mint: Pubkey,
    base_mint: Pubkey,
    token_vault: Pubkey,
    total_deposited: u64,
    total_withdrawn: u64,
    reward_supply: u64,
    minted_supply: u64,
    finalized: u8,
    kicked: u8,
    /// Sum of the raw principal of every voter who currently backs a proposal.
    /// Quorum is valid once this exceeds half the outstanding principal.
    total_voted_principal: u64,
    /// Sum of the `floor(log2(hold)) * principal` weight of every live ballot.
    /// A proposal wins when its support exceeds half of this.
    total_cast_weight: u64,
}

impl GenesisConfig {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < GENESIS_CFG_SIZE || data[..8] != GENESIS_CFG_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let finalized = data[136];
        let kicked = data[137];
        if finalized > 1 || kicked > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            base_mint: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            token_vault: Pubkey::new_from_array(data[72..104].try_into().unwrap()),
            total_deposited: u64::from_le_bytes(data[104..112].try_into().unwrap()),
            total_withdrawn: u64::from_le_bytes(data[112..120].try_into().unwrap()),
            reward_supply: u64::from_le_bytes(data[120..128].try_into().unwrap()),
            minted_supply: u64::from_le_bytes(data[128..136].try_into().unwrap()),
            finalized,
            kicked,
            total_voted_principal: u64::from_le_bytes(data[144..152].try_into().unwrap()),
            total_cast_weight: u64::from_le_bytes(data[152..160].try_into().unwrap()),
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&GENESIS_CFG_DISC);
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[40..72].copy_from_slice(self.base_mint.as_ref());
        data[72..104].copy_from_slice(self.token_vault.as_ref());
        data[104..112].copy_from_slice(&self.total_deposited.to_le_bytes());
        data[112..120].copy_from_slice(&self.total_withdrawn.to_le_bytes());
        data[120..128].copy_from_slice(&self.reward_supply.to_le_bytes());
        data[128..136].copy_from_slice(&self.minted_supply.to_le_bytes());
        data[136] = self.finalized;
        data[137] = self.kicked;
        data[138..144].fill(0);
        data[144..152].copy_from_slice(&self.total_voted_principal.to_le_bytes());
        data[152..160].copy_from_slice(&self.total_cast_weight.to_le_bytes());
    }

    fn is_finalized(&self) -> bool {
        self.finalized == 1
    }

    fn is_kicked(&self) -> bool {
        self.kicked == 1
    }

    fn outstanding_principal(&self) -> u64 {
        self.total_deposited.saturating_sub(self.total_withdrawn)
    }
}

struct GenesisPosition {
    owner: Pubkey,
    amount: u64,
    withdrawn: u64,
    /// Slot of the depositor's most recent deposit (last-write-time). Vote weight
    /// is `floor(log2(vote_slot - start_slot)) * staked`. Cleared to 0 on exit.
    start_slot: u64,
    /// The single proposal this voter currently backs (one vote, one proposal);
    /// `Pubkey::default()` means no live ballot. Withdrawal during voting requires
    /// no live ballot, so a voter must retract before exiting.
    voted_proposal: Pubkey,
    /// The log-weight and raw principal this position contributes to `voted_proposal`
    /// (recorded so a retract/move can back the exact amounts out of the tallies).
    voted_weight: u64,
    voted_principal: u64,
}

impl GenesisPosition {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < GENESIS_POSITION_SIZE || data[..8] != GENESIS_POSITION_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            owner: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            amount: u64::from_le_bytes(data[40..48].try_into().unwrap()),
            withdrawn: u64::from_le_bytes(data[48..56].try_into().unwrap()),
            start_slot: u64::from_le_bytes(data[56..64].try_into().unwrap()),
            voted_proposal: Pubkey::new_from_array(data[64..96].try_into().unwrap()),
            voted_weight: u64::from_le_bytes(data[96..104].try_into().unwrap()),
            voted_principal: u64::from_le_bytes(data[104..112].try_into().unwrap()),
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&GENESIS_POSITION_DISC);
        data[8..40].copy_from_slice(self.owner.as_ref());
        data[40..48].copy_from_slice(&self.amount.to_le_bytes());
        data[48..56].copy_from_slice(&self.withdrawn.to_le_bytes());
        data[56..64].copy_from_slice(&self.start_slot.to_le_bytes());
        data[64..96].copy_from_slice(self.voted_proposal.as_ref());
        data[96..104].copy_from_slice(&self.voted_weight.to_le_bytes());
        data[104..112].copy_from_slice(&self.voted_principal.to_le_bytes());
    }

    fn staked(&self) -> u64 {
        self.amount.saturating_sub(self.withdrawn)
    }

    fn has_live_ballot(&self) -> bool {
        self.voted_proposal != Pubkey::default()
    }
}

/// Time-weighted vote power: `floor(log2(age)) * staked`, where `age` is the
/// position's age in slots at vote time. Younger than 2 slots (log2 == 0) has no
/// weight, so there is monotonic pressure to deposit earlier.
fn genesis_vote_weight(staked: u64, age: u64) -> u64 {
    if staked == 0 || age < 2 {
        return 0;
    }
    (age.ilog2() as u64).saturating_mul(staked)
}

struct GenesisDistribution {
    genesis_cfg: Pubkey,
    destination: Pubkey,
    proposal_id: u64,
    /// Sum of `floor(log2(hold)) * principal` of every voter backing this proposal.
    support_weight: u64,
    /// Sum of the raw principal of every voter backing this proposal.
    support_principal: u64,
    /// 1 once this proposal has been triggered (it won and minted the full supply).
    executed: u8,
}

impl GenesisDistribution {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < GENESIS_DISTRIBUTION_SIZE || data[..8] != GENESIS_DISTRIBUTION_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let executed = data[96];
        if executed > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            genesis_cfg: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            destination: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            proposal_id: u64::from_le_bytes(data[72..80].try_into().unwrap()),
            support_weight: u64::from_le_bytes(data[80..88].try_into().unwrap()),
            support_principal: u64::from_le_bytes(data[88..96].try_into().unwrap()),
            executed,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&GENESIS_DISTRIBUTION_DISC);
        data[8..40].copy_from_slice(self.genesis_cfg.as_ref());
        data[40..72].copy_from_slice(self.destination.as_ref());
        data[72..80].copy_from_slice(&self.proposal_id.to_le_bytes());
        data[80..88].copy_from_slice(&self.support_weight.to_le_bytes());
        data[88..96].copy_from_slice(&self.support_principal.to_le_bytes());
        data[96] = self.executed;
        data[97..GENESIS_DISTRIBUTION_SIZE].fill(0);
    }

    fn is_executed(&self) -> bool {
        self.executed == 1
    }
}

struct BuilderApproval {
    coin_mint: Pubkey,
    builder_program: Pubkey,
    code_hash: [u8; 32],
    terms_hash: [u8; 32],
    approved_slot: u64,
    enabled: u8,
}

impl BuilderApproval {
    fn deserialize(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < BUILDER_APPROVAL_SIZE || data[..8] != BUILDER_APPROVAL_DISC {
            return Err(ProgramError::InvalidAccountData);
        }
        let enabled = data[144];
        if enabled > 1 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            coin_mint: Pubkey::new_from_array(data[8..40].try_into().unwrap()),
            builder_program: Pubkey::new_from_array(data[40..72].try_into().unwrap()),
            code_hash: data[72..104].try_into().unwrap(),
            terms_hash: data[104..136].try_into().unwrap(),
            approved_slot: u64::from_le_bytes(data[136..144].try_into().unwrap()),
            enabled,
        })
    }

    fn serialize(&self, data: &mut [u8]) {
        data[..8].copy_from_slice(&BUILDER_APPROVAL_DISC);
        data[8..40].copy_from_slice(self.coin_mint.as_ref());
        data[40..72].copy_from_slice(self.builder_program.as_ref());
        data[72..104].copy_from_slice(&self.code_hash);
        data[104..136].copy_from_slice(&self.terms_hash);
        data[136..144].copy_from_slice(&self.approved_slot.to_le_bytes());
        data[144] = self.enabled;
        data[145..BUILDER_APPROVAL_SIZE].fill(0);
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
    create_pda_account_with_owner(
        payer,
        target,
        system_program,
        program_id,
        seeds,
        size,
        program_id,
    )
}

fn create_pda_account_with_owner<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    pda_program_id: &Pubkey,
    seeds: &[&[u8]],
    size: usize,
    owner: &Pubkey,
) -> ProgramResult {
    let (expected, bump) = Pubkey::find_program_address(seeds, pda_program_id);
    if *target.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(size);
    let mut seeds_with_bump: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::from(seeds);
    let bump_bytes = [bump];
    seeds_with_bump.push(&bump_bytes);
    invoke_signed(
        &system_instruction::create_account(payer.key, target.key, lamports, size as u64, owner),
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

fn verify_percolator_program(percolator_program: &AccountInfo) -> ProgramResult {
    if *percolator_program.key != percolator_abi::id() {
        msg!("Unexpected Percolator program id");
        return Err(ProgramError::IncorrectProgramId);
    }
    Ok(())
}

fn verify_market_admin_pda(
    market_admin: &AccountInfo,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<u8, ProgramError> {
    let seeds = market_admin_seeds(coin_mint);
    let (expected, bump) = Pubkey::find_program_address(&seeds, program_id);
    if *market_admin.key != expected {
        msg!("Percolator market admin PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }
    Ok(bump)
}

fn ensure_market_admin_account<'a>(
    payer: &AccountInfo<'a>,
    market_admin: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> ProgramResult {
    let seeds = market_admin_seeds(coin_mint);
    let (expected, _) = Pubkey::find_program_address(&seeds, program_id);
    if *market_admin.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if market_admin.lamports() == 0 {
        create_pda_account_with_owner(
            payer,
            market_admin,
            system_program,
            program_id,
            &seeds,
            0,
            &solana_program::system_program::ID,
        )?;
    } else if market_admin.owner != &solana_program::system_program::ID
        || market_admin.data_len() != 0
    {
        msg!("Percolator market admin PDA must be a system account");
        return Err(ProgramError::IllegalOwner);
    }
    Ok(())
}

fn percolator_admin_tag_allowed(tag: u8) -> bool {
    matches!(
        tag,
        PERC_IX_CLOSE_SLAB
            | PERC_IX_RESOLVE_MARKET
            | PERC_IX_UPDATE_INSURANCE_POLICY
            | PERC_IX_CONFIGURE_HYBRID_ORACLE
            | PERC_IX_CONFIGURE_EWMA_MARK
            | PERC_IX_UPDATE_LIQUIDATION_FEE_POLICY
            | PERC_IX_CONFIGURE_PERMISSIONLESS_RESOLVE
            | PERC_IX_UPDATE_ASSET_LIFECYCLE
            | PERC_IX_UPDATE_MAINTENANCE_FEE_POLICY
            | PERC_IX_UPDATE_BACKING_FEE_POLICY
            | PERC_IX_UPDATE_TRADE_FEE_POLICY
            | PERC_IX_UPDATE_FEE_REDIRECT_POLICY
            | PERC_IX_UPDATE_MARKET_INIT_FEE_POLICY
            | PERC_IX_UPDATE_BASE_UNIT_MINTS
            | PERC_IX_CONFIGURE_AUTH_MARK
    )
}

fn account_meta_from_info(
    account: &AccountInfo,
    is_signer: bool,
) -> solana_program::instruction::AccountMeta {
    if account.is_writable {
        solana_program::instruction::AccountMeta::new(*account.key, is_signer)
    } else {
        solana_program::instruction::AccountMeta::new_readonly(*account.key, is_signer)
    }
}

fn load_percolator_market_config(
    market_slab: &AccountInfo,
    expected_collateral_mint: &Pubkey,
) -> Result<percolator_abi::MarketConfig, ProgramError> {
    if market_slab.owner != &percolator_abi::id() {
        msg!("Market slab must be owned by Percolator");
        return Err(ProgramError::IllegalOwner);
    }
    let slab_data = market_slab.try_borrow_data()?;
    let config = percolator_abi::read_market_config(&slab_data)?;
    if config.collateral_mint != expected_collateral_mint.to_bytes() {
        msg!("Percolator slab collateral mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(config)
}

fn validate_percolator_vault_accounts(
    market_slab: &AccountInfo,
    percolator_vault: &AccountInfo,
    percolator_vault_pda: &AccountInfo,
    collateral_mint: &Pubkey,
) -> ProgramResult {
    let (expected_vault_authority, _) =
        Pubkey::find_program_address(&[b"vault", market_slab.key.as_ref()], &percolator_abi::id());
    if *percolator_vault_pda.key != expected_vault_authority {
        msg!("Percolator vault authority PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }
    validate_token_account(percolator_vault, collateral_mint, &expected_vault_authority)
}

fn verify_genesis_config_pda(
    genesis_cfg: &AccountInfo,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<GenesisConfig, ProgramError> {
    let (expected, _) = Pubkey::find_program_address(&genesis_cfg_seeds(coin_mint), program_id);
    if *genesis_cfg.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if genesis_cfg.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let data = genesis_cfg.try_borrow_data()?;
    let cfg = GenesisConfig::deserialize(&data)?;
    if cfg.coin_mint != *coin_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(cfg)
}

fn verify_genesis_vault(
    genesis_vault: &AccountInfo,
    cfg: &GenesisConfig,
    market_admin: &Pubkey,
    program_id: &Pubkey,
) -> ProgramResult {
    let (expected_vault, _) =
        Pubkey::find_program_address(&genesis_vault_seeds(&cfg.coin_mint), program_id);
    if *genesis_vault.key != expected_vault || cfg.token_vault != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    validate_token_account(genesis_vault, &cfg.base_mint, market_admin)
}

fn genesis_recoverable_principal(
    remaining_principal: u64,
    vault_balance: u64,
    outstanding_principal: u64,
) -> Result<u64, ProgramError> {
    if remaining_principal == 0 {
        return Ok(0);
    }
    if outstanding_principal == 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    if vault_balance >= outstanding_principal {
        return Ok(remaining_principal);
    }
    Ok(((remaining_principal as u128)
        .checked_mul(vault_balance as u128)
        .ok_or(ProgramError::ArithmeticOverflow)?
        / outstanding_principal as u128) as u64)
}

fn genesis_recovery_ix_data(
    kind: u8,
    domain: u8,
    amount: u64,
) -> Result<alloc::vec::Vec<u8>, ProgramError> {
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let amount = amount as u128;
    let mut ix_data = alloc::vec::Vec::with_capacity(18);
    match kind {
        GENESIS_RECOVER_INSURANCE_LIMITED => {
            ix_data.push(PERC_IX_WITHDRAW_INSURANCE_LIMITED);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_BACKING => {
            ix_data.push(PERC_IX_WITHDRAW_BACKING_BUCKET);
            ix_data.push(domain);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_BACKING_EARNINGS => {
            ix_data.push(PERC_IX_WITHDRAW_BACKING_BUCKET_EARNINGS);
            ix_data.push(domain);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_INSURANCE_TERMINAL => {
            ix_data.push(PERC_IX_WITHDRAW_INSURANCE);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        GENESIS_RECOVER_INSURANCE_DOMAIN => {
            ix_data.push(PERC_IX_WITHDRAW_INSURANCE_DOMAIN);
            ix_data.push(domain);
            ix_data.extend_from_slice(&amount.to_le_bytes());
        }
        _ => return Err(ProgramError::InvalidInstructionData),
    }
    Ok(ix_data)
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
    if amount == 0 {
        return Ok(());
    }
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
        &[
            coin_mint.clone(),
            destination.clone(),
            mint_authority.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )
}

/// Verify CoinConfig PDA and return authority.
fn load_coin_config(
    coin_cfg_account: &AccountInfo,
    coin_mint: &Pubkey,
    program_id: &Pubkey,
) -> Result<CoinConfig, ProgramError> {
    let (expected_cfg, _) = Pubkey::find_program_address(&coin_cfg_seeds(coin_mint), program_id);
    if *coin_cfg_account.key != expected_cfg {
        return Err(ProgramError::InvalidSeeds);
    }
    if coin_cfg_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let cfg_data = coin_cfg_account.try_borrow_data()?;
    CoinConfig::deserialize(&cfg_data)
}

fn require_live(coin_cfg: &CoinConfig) -> ProgramResult {
    if !coin_cfg.is_live() {
        msg!("COIN bootstrap phase is not live");
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
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
        IX_INIT_COIN_CONFIG => process_init_coin_config(program_id, accounts, &mut data),
        IX_MINT_REWARD => process_mint_reward(program_id, accounts, &mut data),
        IX_TRANSFER_MINT_AUTHORITY => process_transfer_mint_authority(program_id, accounts),
        IX_ACTIVATE_LIVE => process_activate_live(program_id, accounts, &mut data),
        IX_INIT_PERCOLATOR_MARKET => process_init_percolator_market(program_id, accounts, &data),
        IX_PERCOLATOR_ADMIN => process_percolator_admin(program_id, accounts, &data),
        IX_INIT_GENESIS_BOOTSTRAP => {
            process_init_genesis_bootstrap(program_id, accounts, &mut data)
        }
        IX_GENESIS_DEPOSIT => process_genesis_deposit(program_id, accounts, &mut data),
        IX_GENESIS_WITHDRAW => process_genesis_withdraw(program_id, accounts, &mut data),
        IX_TRIGGER_GENESIS_DISTRIBUTION => {
            process_trigger_genesis_distribution(program_id, accounts, data)
        }
        IX_FINALIZE_GENESIS => process_finalize_genesis(program_id, accounts, &mut data),
        IX_DRAW_GENESIS_SURPLUS => process_draw_genesis_surplus(program_id, accounts, &mut data),
        IX_KICKSTART_GENESIS_MARKET => {
            process_kickstart_genesis_market(program_id, accounts, &mut data)
        }
        IX_RECOVER_GENESIS_MARKET => {
            process_recover_genesis_market(program_id, accounts, &mut data)
        }
        IX_INIT_GENESIS_DISTRIBUTION => {
            process_init_genesis_distribution(program_id, accounts, &mut data)
        }
        IX_VOTE_GENESIS_DISTRIBUTION => {
            process_vote_genesis_distribution(program_id, accounts, &mut data)
        }
        IX_APPROVE_BUILDER => process_approve_builder(program_id, accounts, &mut data),
        IX_INIT_GENESIS_SQUADS => process_init_genesis_squads(program_id, accounts, &mut data),
        IX_HANDOVER_GENESIS_SQUADS => {
            process_handover_genesis_squads(program_id, accounts, &mut data)
        }
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
//
// Data: bootstrap_delay_slots (u64, optional for legacy zero-delay callers)

fn process_init_coin_config<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
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
    let bootstrap_delay_slots = read_optional_u64(data)?;
    let bootstrap_start_slot = Clock::get()?.slot;
    if bootstrap_start_slot
        .checked_add(bootstrap_delay_slots)
        .is_none()
    {
        msg!("bootstrap delay overflows slot range");
        return Err(ProgramError::InvalidInstructionData);
    }

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
    let (expected_mint_auth, _) =
        Pubkey::find_program_address(&mint_authority_seeds(coin_mint.key), program_id);
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
    create_pda_account(
        payer,
        coin_cfg_account,
        system_program,
        program_id,
        &seeds,
        COIN_CFG_SIZE,
    )?;

    let mut cfg_data = coin_cfg_account.try_borrow_mut_data()?;
    let (phase, live_slot) = if bootstrap_delay_slots == 0 {
        (PHASE_LIVE, bootstrap_start_slot)
    } else {
        (PHASE_BOOTSTRAP, 0)
    };
    let cfg = CoinConfig {
        authority: *authority.key,
        bootstrap_start_slot,
        bootstrap_delay_slots,
        live_slot,
        phase,
    };
    cfg.serialize(&mut cfg_data);

    Ok(())
}

// ============================================================================
// activate_live
// ============================================================================
// Accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, read-only governance PDA — must match CoinConfig.authority)
//   [2] coin_mint (read-only)
//   [3] coin_config PDA (writable)
//   [4] clock

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
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;

    let mut cfg_data = coin_cfg_account.try_borrow_mut_data()?;
    let mut coin_cfg = CoinConfig::deserialize(&cfg_data)?;
    let (expected_cfg, _) =
        Pubkey::find_program_address(&coin_cfg_seeds(coin_mint.key), program_id);
    if *coin_cfg_account.key != expected_cfg {
        return Err(ProgramError::InvalidSeeds);
    }
    if coin_cfg_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if coin_cfg.is_live() {
        return Ok(());
    }

    let clock = Clock::from_account_info(clock_info)?;
    let live_after_slot = coin_cfg
        .bootstrap_start_slot
        .checked_add(coin_cfg.bootstrap_delay_slots)
        .ok_or(ProgramError::InvalidAccountData)?;
    if clock.slot < live_after_slot {
        msg!("bootstrap delay has not elapsed");
        return Err(ProgramError::InvalidInstructionData);
    }

    coin_cfg.phase = PHASE_LIVE;
    coin_cfg.live_slot = clock.slot;
    coin_cfg.serialize(&mut cfg_data);
    Ok(())
}

// ============================================================================
// governed reward mint lifecycle
// ============================================================================

fn process_mint_reward<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;
    let destination_token = load_token_account(destination)?;
    if destination_token.mint != *coin_mint.key {
        msg!("Reward destination mint mismatch");
        return Err(ProgramError::InvalidAccountData);
    }

    let ma_seeds = mint_authority_seeds(coin_mint.key);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", coin_mint.key.as_ref(), &bump_bytes];
    mint_coin(
        token_program,
        coin_mint,
        destination,
        mint_authority,
        amount,
        &signer_seeds,
    )
}

fn process_transfer_mint_authority<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let new_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    verify_token_program(token_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;

    let ma_seeds = mint_authority_seeds(coin_mint.key);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }

    let new_authority_opt = if *new_authority.key == Pubkey::default() {
        None
    } else {
        Some(new_authority.key)
    };
    let ix = spl_token::instruction::set_authority(
        token_program.key,
        coin_mint.key,
        new_authority_opt,
        spl_token::instruction::AuthorityType::MintTokens,
        mint_authority.key,
        &[],
    )?;
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", coin_mint.key.as_ref(), &bump_bytes];
    invoke_signed(
        &ix,
        &[
            coin_mint.clone(),
            mint_authority.clone(),
            token_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// ============================================================================
// Percolator market lifecycle wiring
// ============================================================================
// init_percolator_market accounts:
//   [0] payer/user (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] market_admin PDA (writable; created if missing)
//   [4] market_slab (writable; owned by Percolator, created by caller)
//   [5] collateral_mint
//   [6] percolator_program
//   [7] system_program
//
// Data: raw Percolator InitMarket instruction data, including tag 0.

fn process_init_percolator_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    percolator_ix_data: &[u8],
) -> ProgramResult {
    if percolator_ix_data.first().copied() != Some(PERC_IX_INIT_MARKET) {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let collateral_mint = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_percolator_program(percolator_program)?;
    let _coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    ensure_market_admin_account(
        payer,
        market_admin,
        system_program,
        coin_mint.key,
        program_id,
    )?;
    if market_slab.owner != &percolator_abi::id() {
        return Err(ProgramError::IllegalOwner);
    }
    if collateral_mint.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let mint_data = collateral_mint.try_borrow_data()?;
    spl_token::state::Mint::unpack(&mint_data)?;
    drop(mint_data);

    let (_, admin_bump) =
        Pubkey::find_program_address(&market_admin_seeds(coin_mint.key), program_id);
    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: alloc::vec![
            account_meta_from_info(market_admin, true),
            solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*collateral_mint.key, false),
        ],
        data: percolator_ix_data.to_vec(),
    };
    invoke_signed(
        &ix,
        &[
            market_admin.clone(),
            market_slab.clone(),
            collateral_mint.clone(),
            percolator_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// percolator_admin accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] market_admin PDA (first Percolator account; signed via invoke_signed)
//   [5] percolator_program
//   [6..] remaining Percolator accounts after the admin/authority account
//
// Data: raw allowed Percolator admin/lifecycle instruction data.

fn process_percolator_admin<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    percolator_ix_data: &[u8],
) -> ProgramResult {
    let tag = percolator_ix_data
        .first()
        .copied()
        .ok_or(ProgramError::InvalidInstructionData)?;
    if !percolator_admin_tag_allowed(tag) {
        msg!("Percolator instruction is not an allowed futarchy admin lifecycle action");
        return Err(ProgramError::InvalidInstructionData);
    }

    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_percolator_program(percolator_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;

    let tail: alloc::vec::Vec<AccountInfo<'a>> = iter.cloned().collect();
    let mut metas = alloc::vec::Vec::with_capacity(1 + tail.len());
    metas.push(account_meta_from_info(market_admin, true));
    for account in tail.iter() {
        metas.push(account_meta_from_info(account, account.is_signer));
    }
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: metas,
        data: percolator_ix_data.to_vec(),
    };

    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];
    let mut cpi_accounts = alloc::vec::Vec::with_capacity(2 + tail.len());
    cpi_accounts.push(market_admin.clone());
    cpi_accounts.extend(tail);
    cpi_accounts.push(percolator_program.clone());
    invoke_signed(&ix, &cpi_accounts, &[&signer_seeds])
}

// ============================================================================
// genesis bootstrap — base deposits, vote units, reward cap, and kickoff
// ============================================================================
// init_genesis_bootstrap accounts:
//   [0] payer (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] base_mint
//   [5] genesis_config PDA (writable, to create)
//   [6] genesis_vault PDA (writable, to create; SPL token account)
//   [7] market_admin PDA (writable; created if missing)
//   [8] token_program
//   [9] rent sysvar
//   [10] system_program
//
// Data: reward_supply (u64)

fn process_init_genesis_bootstrap<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let base_mint = next_account_info(iter)?;
    let genesis_cfg = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let reward_supply = read_u64(data)?;
    if reward_supply == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if coin_cfg.is_live() {
        msg!("genesis bootstrap must be initialized during bootstrap phase");
        return Err(ProgramError::InvalidInstructionData);
    }
    let clock = Clock::get()?;
    let live_after_slot = coin_cfg
        .bootstrap_start_slot
        .checked_add(coin_cfg.bootstrap_delay_slots)
        .ok_or(ProgramError::InvalidAccountData)?;
    if clock.slot >= live_after_slot {
        msg!("genesis bootstrap delay has elapsed");
        return Err(ProgramError::InvalidInstructionData);
    }
    if base_mint.owner != &spl_token::ID {
        return Err(ProgramError::IllegalOwner);
    }
    let base_mint_data = base_mint.try_borrow_data()?;
    spl_token::state::Mint::unpack(&base_mint_data)?;
    drop(base_mint_data);

    ensure_market_admin_account(
        payer,
        market_admin,
        system_program,
        coin_mint.key,
        program_id,
    )?;
    let genesis_cfg_seeds_arr = genesis_cfg_seeds(coin_mint.key);
    create_pda_account(
        payer,
        genesis_cfg,
        system_program,
        program_id,
        &genesis_cfg_seeds_arr,
        GENESIS_CFG_SIZE,
    )?;

    let genesis_vault_seeds_arr = genesis_vault_seeds(coin_mint.key);
    let (expected_vault, vault_bump) =
        Pubkey::find_program_address(&genesis_vault_seeds_arr, program_id);
    if *genesis_vault.key != expected_vault {
        return Err(ProgramError::InvalidSeeds);
    }
    let vault_bump_bytes = [vault_bump];
    let vault_signer: [&[u8]; 3] = [b"genesis_vault", coin_mint.key.as_ref(), &vault_bump_bytes];
    let rent = Rent::get()?;
    invoke_signed(
        &system_instruction::create_account(
            payer.key,
            genesis_vault.key,
            rent.minimum_balance(spl_token::state::Account::LEN),
            spl_token::state::Account::LEN as u64,
            &spl_token::ID,
        ),
        &[payer.clone(), genesis_vault.clone(), system_program.clone()],
        &[&vault_signer],
    )?;
    let init_ix = spl_token::instruction::initialize_account2(
        token_program.key,
        genesis_vault.key,
        base_mint.key,
        market_admin.key,
    )?;
    invoke(
        &init_ix,
        &[
            genesis_vault.clone(),
            base_mint.clone(),
            rent_sysvar.clone(),
            token_program.clone(),
        ],
    )?;

    let cfg = GenesisConfig {
        coin_mint: *coin_mint.key,
        base_mint: *base_mint.key,
        token_vault: *genesis_vault.key,
        total_deposited: 0,
        total_withdrawn: 0,
        reward_supply,
        minted_supply: 0,
        finalized: 0,
        kicked: 0,
        total_voted_principal: 0,
        total_cast_weight: 0,
    };
    let mut cfg_data = genesis_cfg.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

// genesis_deposit accounts:
//   [0] user (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] genesis_config PDA (writable)
//   [4] genesis_position PDA (writable, created if missing)
//   [5] user_base_ata (writable)
//   [6] genesis_vault (writable)
//   [7] token_program
//   [8] system_program
//
// Data: amount (u64). One base unit deposited equals one vote unit.

fn process_genesis_deposit<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let genesis_position = next_account_info(iter)?;
    let user_base_ata = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    // Joining is open any time during the pre-deployment deposit phase — no fixed
    // window — but closes once the pooled principal is deployed at kickstart. This
    // keeps the invariant that every base unit is either in the genesis vault
    // (pre-kickstart) or in the market (post-kickstart), never split, so a
    // depositor's bootstrap exit always draws from the right place.
    if coin_cfg.is_live() {
        msg!("genesis deposits close once voting starts");
        return Err(ProgramError::InvalidInstructionData);
    }
    let clock = Clock::get()?;

    let mut genesis_cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    let mut genesis_cfg = GenesisConfig::deserialize(&genesis_cfg_data)?;
    if genesis_cfg.coin_mint != *coin_mint.key
        || *genesis_cfg_account.key
            != Pubkey::find_program_address(&genesis_cfg_seeds(coin_mint.key), program_id).0
    {
        return Err(ProgramError::InvalidSeeds);
    }
    if genesis_cfg.is_finalized() || genesis_cfg.is_kicked() {
        msg!("genesis deposits close once the pooled capital is deployed at kickstart");
        return Err(ProgramError::InvalidInstructionData);
    }
    let (market_admin, _) =
        Pubkey::find_program_address(&market_admin_seeds(coin_mint.key), program_id);
    verify_genesis_vault(genesis_vault, &genesis_cfg, &market_admin, program_id)?;
    validate_token_account(user_base_ata, &genesis_cfg.base_mint, user.key)?;

    let position_seeds = genesis_position_seeds(genesis_cfg_account.key, user.key);
    let (expected_position, _) = Pubkey::find_program_address(&position_seeds, program_id);
    if *genesis_position.key != expected_position {
        return Err(ProgramError::InvalidSeeds);
    }
    let mut pos = if genesis_position.data_len() == 0 || genesis_position.lamports() == 0 {
        create_pda_account(
            user,
            genesis_position,
            system_program,
            program_id,
            &position_seeds,
            GENESIS_POSITION_SIZE,
        )?;
        GenesisPosition {
            owner: *user.key,
            amount: 0,
            withdrawn: 0,
            start_slot: 0,
            voted_proposal: Pubkey::default(),
            voted_weight: 0,
            voted_principal: 0,
        }
    } else {
        if genesis_position.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let pos_data = genesis_position.try_borrow_data()?;
        let pos = GenesisPosition::deserialize(&pos_data)?;
        if pos.owner != *user.key {
            return Err(ProgramError::IllegalOwner);
        }
        pos
    };

    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        user_base_ata.key,
        genesis_vault.key,
        user.key,
        &[],
        amount,
    )?;
    invoke(
        &xfer_ix,
        &[
            user_base_ata.clone(),
            genesis_vault.clone(),
            user.clone(),
            token_program.clone(),
        ],
    )?;

    genesis_cfg.total_deposited = genesis_cfg
        .total_deposited
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    pos.amount = pos
        .amount
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    // Last-write-time: each deposit resets the clock used for vote weighting.
    pos.start_slot = clock.slot;

    genesis_cfg.serialize(&mut genesis_cfg_data);
    let mut pos_data = genesis_position.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    Ok(())
}

// genesis_withdraw accounts:
//   [0] user (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] genesis_config PDA (writable)
//   [4] genesis_position PDA (writable)
//   [5] user_base_ata (writable)
//   [6] genesis_vault (writable)
//   [7] market_admin PDA
//   [8] token_program
//
// Data: backing_domain (u8), insurance_pull (u64), backing_pull (u64).
// The pull fields are retained for ABI stability but must now be 0 — withdraw no
// longer touches the market (see below).
//
// One permissionless withdraw for every phase; it always forfeits the vote
// (start_slot -> 0). A voter must retract before exiting during voting, so a vote
// counts only if held through finalization. By phase:
//   - Before kickstart: capital is still in the genesis vault; refund the full
//     remaining principal and shrink the pool.
//   - After kickstart (the live decision window OR after finalization): pay the
//     remaining principal pro-rata against the genesis_vault's health ratio and
//     settle the FULL claim, so every depositor recovers the same share (up to
//     sub-unit integer-division dust, which accrues to later exiters — the safe
//     direction) regardless of withdrawal order, and looping pays nothing.
//
// Withdraw never pulls from the market itself. Bringing deployed capital back into
// the shared genesis_vault is the (futarchy-gated) recover_genesis_market step —
// decoupling recovery (which fills the shared vault, helping everyone equally) from
// withdrawal (which takes a pro-rata share) is what removes the first-come-first-
// served bank run that the old in-line market pull allowed. NOTE: this means a
// depositor exiting before governance has recovered market capital realizes the
// un-recovered remainder as loss against the full-claim settlement — the exit-anytime
// vs. keep-capital-at-risk tension is a governance design choice, left as-is here.
fn process_genesis_withdraw<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let user = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let genesis_position = next_account_info(iter)?;
    let user_base_ata = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let backing_domain = read_u8(data)?;
    let insurance_pull = read_u64(data)?;
    let backing_pull = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !user.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    validate_token_account(user_base_ata, &cfg.base_mint, user.key)?;

    let position_seeds = genesis_position_seeds(genesis_cfg_account.key, user.key);
    if *genesis_position.key != Pubkey::find_program_address(&position_seeds, program_id).0 {
        return Err(ProgramError::InvalidSeeds);
    }
    if genesis_position.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let pos_data = genesis_position.try_borrow_data()?;
    let mut pos = GenesisPosition::deserialize(&pos_data)?;
    drop(pos_data);
    if pos.owner != *user.key {
        return Err(ProgramError::IllegalOwner);
    }
    // Exit is free at any time EXCEPT with live ballots during voting: a voter must
    // first retract (give up) every vote, so a cast ballot can never outlive the
    // capital that backed it. Non-voters can leave whenever they like.
    if coin_cfg.is_live() && !cfg.is_finalized() && pos.has_live_ballot() {
        msg!("retract your vote before exiting during voting");
        return Err(ProgramError::InvalidInstructionData);
    }
    let remaining = pos.amount.saturating_sub(pos.withdrawn);

    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];

    // Withdraw never pulls from the market. The pull fields are retained for ABI
    // stability but must be 0; bringing deployed capital back into the shared
    // genesis_vault is the (futarchy-gated) recover_genesis_market step. Decoupling
    // recovery (fills the shared vault, helping everyone equally) from withdrawal
    // (takes a pro-rata share) is what removes the first-come-first-served bank run.
    if insurance_pull != 0 || backing_pull != 0 {
        msg!("withdraw no longer pulls from the market; crank recover_genesis_market first");
        return Err(ProgramError::InvalidInstructionData);
    }
    let _ = backing_domain;
    if iter.next().is_some() {
        return Err(ProgramError::InvalidInstructionData);
    }

    if !cfg.is_kicked() {
        // Pre-kickstart: capital is still in the genesis vault. Refund the full
        // remaining principal and shrink the pool so a later kickstart deploys the
        // right amount.
        if remaining > 0 {
            let vault_balance = load_token_account(genesis_vault)?.amount;
            let actual = core::cmp::min(remaining, vault_balance);
            if actual > 0 {
                let xfer_ix = spl_token::instruction::transfer(
                    token_program.key,
                    genesis_vault.key,
                    user_base_ata.key,
                    market_admin.key,
                    &[],
                    actual,
                )?;
                invoke_signed(
                    &xfer_ix,
                    &[
                        genesis_vault.clone(),
                        user_base_ata.clone(),
                        market_admin.clone(),
                        token_program.clone(),
                    ],
                    &[&signer_seeds],
                )?;
            }
            cfg.total_deposited = cfg.total_deposited.saturating_sub(actual);
            pos.amount = pos.amount.saturating_sub(actual);
        }
    } else {
        // Kicked — the live decision window OR post-finalization. Pay the remaining
        // principal pro-rata against the genesis_vault's health ratio and settle the
        // FULL claim. Retiring the whole `remaining` (not just the paid `actual`)
        // keeps the vault/outstanding ratio invariant, so every depositor recovers
        // the same share regardless of withdrawal order (exact in real arithmetic; up
        // to sub-unit flooring dust that accrues to later exiters — the safe
        // direction) and looping pays nothing. The unpaid shortfall is this
        // depositor's realized share of the market loss.
        // Capital still deployed in the market must first be recovered to the shared
        // vault via the (futarchy-gated) recover_genesis_market — exiting before
        // recovery realizes the un-recovered remainder as loss.
        if remaining > 0 {
            let outstanding = cfg.outstanding_principal();
            if outstanding == 0 {
                return Err(ProgramError::InvalidAccountData);
            }
            let vault_balance = load_token_account(genesis_vault)?.amount;
            let actual = genesis_recoverable_principal(remaining, vault_balance, outstanding)?;
            if actual > 0 {
                let xfer_ix = spl_token::instruction::transfer(
                    token_program.key,
                    genesis_vault.key,
                    user_base_ata.key,
                    market_admin.key,
                    &[],
                    actual,
                )?;
                invoke_signed(
                    &xfer_ix,
                    &[
                        genesis_vault.clone(),
                        user_base_ata.clone(),
                        market_admin.clone(),
                        token_program.clone(),
                    ],
                    &[&signer_seeds],
                )?;
            }
            cfg.total_withdrawn = cfg
                .total_withdrawn
                .checked_add(remaining)
                .ok_or(ProgramError::ArithmeticOverflow)?;
            pos.withdrawn = pos
                .withdrawn
                .checked_add(remaining)
                .ok_or(ProgramError::ArithmeticOverflow)?;
        }
    }

    // The exit forfeits all voting power regardless of how much was recovered.
    pos.start_slot = 0;

    let mut cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    drop(cfg_data);
    let mut pos_data = genesis_position.try_borrow_mut_data()?;
    pos.serialize(&mut pos_data);
    Ok(())
}

fn require_genesis_governance(
    program_id: &Pubkey,
    payer: &AccountInfo,
    authority: &AccountInfo,
    coin_mint: &AccountInfo,
    coin_cfg_account: &AccountInfo,
) -> Result<CoinConfig, ProgramError> {
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_live(&coin_cfg)?;
    Ok(coin_cfg)
}

// init_genesis_distribution accounts:
//   [0] payer/proposer (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] genesis_config PDA
//   [4] distribution proposal PDA (writable, to create)
//   [5] destination COIN token account
//   [6] system_program
//
// Data: proposal_id (u64). Each proposal is a candidate for the full supply.

fn process_init_genesis_distribution<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let distribution_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    let proposal_id = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    require_live(&coin_cfg)?;
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let destination_token = load_token_account(destination)?;
    if destination_token.mint != *coin_mint.key {
        return Err(ProgramError::InvalidAccountData);
    }

    let proposal_id_bytes = proposal_id.to_le_bytes();
    let seeds = genesis_distribution_seeds(genesis_cfg_account.key, &proposal_id_bytes);
    let expected = Pubkey::find_program_address(&seeds, program_id).0;
    if *distribution_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    create_pda_account(
        payer,
        distribution_account,
        system_program,
        program_id,
        &seeds,
        GENESIS_DISTRIBUTION_SIZE,
    )?;

    let proposal = GenesisDistribution {
        genesis_cfg: *genesis_cfg_account.key,
        destination: *destination.key,
        proposal_id,
        support_weight: 0,
        support_principal: 0,
        executed: 0,
    };
    let mut proposal_data = distribution_account.try_borrow_mut_data()?;
    proposal.serialize(&mut proposal_data);
    Ok(())
}

// vote_genesis_distribution accounts:
//   [0] voter (signer)
//   [1] coin_mint
//   [2] coin_config PDA
//   [3] genesis_config PDA (writable — running global vote tallies)
//   [4] genesis_position PDA (writable — the voter's single ballot)
//   [5] distribution proposal PDA (writable)
//
// Data: action (u8; 1 = back this proposal, 2 = retract). Each voter backs at
// most one proposal; to switch, retract then back the other one.

fn process_vote_genesis_distribution<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let voter = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let genesis_position = next_account_info(iter)?;
    let distribution_account = next_account_info(iter)?;

    let action = read_u8(data)?;
    // 1 = back this proposal, 2 = retract (give up your ballot).
    if action == 0 || action > 2 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if !voter.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    require_live(&coin_cfg)?;
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() {
        return Err(ProgramError::InvalidInstructionData);
    }

    let position_seeds = genesis_position_seeds(genesis_cfg_account.key, voter.key);
    if *genesis_position.key != Pubkey::find_program_address(&position_seeds, program_id).0
        || genesis_position.owner != program_id
    {
        return Err(ProgramError::InvalidSeeds);
    }
    let mut pos = GenesisPosition::deserialize(&genesis_position.try_borrow_data()?)?;
    if pos.owner != *voter.key {
        return Err(ProgramError::InvalidAccountData);
    }

    if distribution_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut proposal =
        GenesisDistribution::deserialize(&distribution_account.try_borrow_data()?)?;
    if proposal.genesis_cfg != *genesis_cfg_account.key || proposal.is_executed() {
        return Err(ProgramError::InvalidAccountData);
    }

    // One vote, one proposal: any prior ballot must be on THIS proposal (re-vote /
    // retract) — to switch proposals, retract the old one first.
    if pos.has_live_ballot() && pos.voted_proposal != *distribution_account.key {
        msg!("retract your existing vote before backing another proposal");
        return Err(ProgramError::InvalidInstructionData);
    }

    // Back the voter's current contribution (if any) out of this proposal and the
    // global tallies, then either clear it (retract) or re-add at current power.
    if pos.has_live_ballot() {
        proposal.support_weight = proposal
            .support_weight
            .checked_sub(pos.voted_weight)
            .ok_or(ProgramError::InvalidAccountData)?;
        proposal.support_principal = proposal
            .support_principal
            .checked_sub(pos.voted_principal)
            .ok_or(ProgramError::InvalidAccountData)?;
        cfg.total_cast_weight = cfg
            .total_cast_weight
            .checked_sub(pos.voted_weight)
            .ok_or(ProgramError::InvalidAccountData)?;
        cfg.total_voted_principal = cfg
            .total_voted_principal
            .checked_sub(pos.voted_principal)
            .ok_or(ProgramError::InvalidAccountData)?;
    } else if action == 2 {
        msg!("no vote to retract");
        return Err(ProgramError::InvalidInstructionData);
    }

    if action == 2 {
        pos.voted_proposal = Pubkey::default();
        pos.voted_weight = 0;
        pos.voted_principal = 0;
    } else {
        // Back this proposal at the current time-weighted power: weight =
        // floor(log2(hold)) * staked, resolved at vote time. A cleared start slot
        // (exited) or a too-young/empty position has no power.
        let current_slot = Clock::get()?.slot;
        let staked = pos.staked();
        let weight = if pos.start_slot == 0 {
            0
        } else {
            genesis_vote_weight(staked, current_slot.saturating_sub(pos.start_slot))
        };
        if weight == 0 {
            msg!("position has no vote weight (exited, unfunded, or deposited too late)");
            return Err(ProgramError::InvalidAccountData);
        }
        proposal.support_weight = proposal
            .support_weight
            .checked_add(weight)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        proposal.support_principal = proposal
            .support_principal
            .checked_add(staked)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        cfg.total_cast_weight = cfg
            .total_cast_weight
            .checked_add(weight)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        cfg.total_voted_principal = cfg
            .total_voted_principal
            .checked_add(staked)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        pos.voted_proposal = *distribution_account.key;
        pos.voted_weight = weight;
        pos.voted_principal = staked;
    }

    proposal.serialize(&mut distribution_account.try_borrow_mut_data()?);
    cfg.serialize(&mut genesis_cfg_account.try_borrow_mut_data()?);
    pos.serialize(&mut genesis_position.try_borrow_mut_data()?);
    Ok(())
}

// trigger_genesis_distribution accounts (PERMISSIONLESS — no governance signer):
//   [0] cranker (signer, anyone — pays fees only)
//   [1] genesis_config PDA (writable)
//   [2] coin_mint (writable)
//   [3] coin_config PDA
//   [4] destination COIN token account (writable)
//   [5] mint_authority PDA
//   [6] distribution proposal PDA (writable)
//   [7] token_program
//
// Data: none. The vote decides; anyone may execute it. The first proposal to
// reach a quorum-valid weighted majority is minted the full supply and wins —
// triggering ends the genesis decision (winner-take-all).

fn process_trigger_genesis_distribution<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &[u8],
) -> ProgramResult {
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let iter = &mut accounts.iter();
    let cranker = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let mint_authority = next_account_info(iter)?;
    let distribution_account = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !cranker.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    require_live(&coin_cfg)?;
    let dest_token = load_token_account(destination)?;
    if dest_token.mint != *coin_mint.key {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() {
        return Err(ProgramError::InvalidInstructionData);
    }
    // COIN can only be distributed once the pooled capital is actually deployed
    // into the market — no minting on a genesis that was never kickstarted.
    if !cfg.is_kicked() {
        msg!("genesis market must be kicked before triggering");
        return Err(ProgramError::InvalidInstructionData);
    }
    // Winner-take-all: the full supply is minted exactly once. A non-zero
    // minted_supply means a proposal already won.
    if cfg.minted_supply != 0 {
        msg!("genesis distribution already decided");
        return Err(ProgramError::InvalidInstructionData);
    }
    if distribution_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    }
    let mut proposal = GenesisDistribution::deserialize(&distribution_account.try_borrow_data()?)?;
    if proposal.genesis_cfg != *genesis_cfg_account.key
        || proposal.destination != *destination.key
        || proposal.is_executed()
    {
        return Err(ProgramError::InvalidAccountData);
    }
    // Quorum: more than half the outstanding principal has cast a vote. Exits
    // shrink `outstanding`, so this bar drops as depositors leave.
    if (cfg.total_voted_principal as u128) * 2 <= cfg.outstanding_principal() as u128 {
        msg!("genesis vote lacks a principal quorum");
        return Err(ProgramError::InvalidInstructionData);
    }
    // Winner: this proposal holds more than half of all cast log-weight.
    if (proposal.support_weight as u128) * 2 <= cfg.total_cast_weight as u128 {
        msg!("proposal lacks a weighted majority");
        return Err(ProgramError::InvalidInstructionData);
    }

    cfg.minted_supply = cfg.reward_supply;
    proposal.executed = 1;

    let ma_seeds = mint_authority_seeds(coin_mint.key);
    let (expected_ma, ma_bump) = Pubkey::find_program_address(&ma_seeds, program_id);
    if *mint_authority.key != expected_ma {
        return Err(ProgramError::InvalidSeeds);
    }
    let bump_bytes = [ma_bump];
    let signer_seeds: [&[u8]; 3] = [b"coin_mint_authority", coin_mint.key.as_ref(), &bump_bytes];
    let supply = cfg.reward_supply;
    cfg.serialize(&mut genesis_cfg_account.try_borrow_mut_data()?);
    proposal.serialize(&mut distribution_account.try_borrow_mut_data()?);
    mint_coin(
        token_program,
        coin_mint,
        destination,
        mint_authority,
        supply,
        &signer_seeds,
    )
}

// finalize_genesis accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] genesis_config PDA (writable)
//   [3] coin_mint
//   [4] coin_config PDA
//
// Data: none

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
    let genesis_cfg_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;

    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_kicked() {
        msg!("genesis market must be kicked before finalization");
        return Err(ProgramError::InvalidInstructionData);
    }
    if cfg.minted_supply != cfg.reward_supply {
        msg!("genesis reward supply is not fully distributed");
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    cfg.finalized = 1;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

// draw_genesis_surplus accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] genesis_config PDA
//   [3] coin_mint
//   [4] coin_config PDA
//   [5] destination base-token account (writable)
//   [6] genesis_vault (writable)
//   [7] market_admin PDA
//   [8] token_program
//
// Data: amount (u64)

fn process_draw_genesis_surplus<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let destination = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let amount = read_u64(data)?;
    if amount == 0 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_token_program(token_program)?;
    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_finalized() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    let dest_token = load_token_account(destination)?;
    if dest_token.mint != cfg.base_mint {
        return Err(ProgramError::InvalidAccountData);
    }
    let vault_balance = load_token_account(genesis_vault)?.amount;
    let available = vault_balance.saturating_sub(cfg.outstanding_principal());
    if amount > available {
        msg!("genesis surplus draw exceeds recovered surplus");
        return Err(ProgramError::InsufficientFunds);
    }

    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];
    let xfer_ix = spl_token::instruction::transfer(
        token_program.key,
        genesis_vault.key,
        destination.key,
        market_admin.key,
        &[],
        amount,
    )?;
    invoke_signed(
        &xfer_ix,
        &[
            genesis_vault.clone(),
            destination.clone(),
            market_admin.clone(),
            token_program.clone(),
        ],
        &[&signer_seeds],
    )
}

// kickstart_genesis_market accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] genesis_config PDA (writable)
//   [5] market_admin PDA
//   [6] market_slab (writable)
//   [7] genesis_vault (writable; source, owned by market_admin PDA)
//   [8] percolator_vault (writable)
//   [9] percolator_vault_pda
//   [10] percolator_program
//   [11] token_program
//
// Data: backing_domain (u8), backing_expiry_slot (u64)

fn process_kickstart_genesis_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
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
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_token_program(token_program)?;
    verify_percolator_program(percolator_program)?;
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    let mut cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if cfg.is_finalized() || cfg.is_kicked() || cfg.total_deposited == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    let percolator_cfg = load_percolator_market_config(market_slab, &cfg.base_mint)?;
    if percolator_cfg.admin != market_admin.key.to_bytes()
        || percolator_cfg.insurance_authority != market_admin.key.to_bytes()
        || percolator_cfg.insurance_operator != market_admin.key.to_bytes()
        || percolator_cfg.backing_bucket_authority != market_admin.key.to_bytes()
    {
        msg!("genesis market must be controlled by the COIN market-admin PDA");
        return Err(ProgramError::InvalidAccountData);
    }
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.base_mint,
    )?;
    let vault_balance = load_token_account(genesis_vault)?.amount;
    if vault_balance < cfg.total_deposited {
        return Err(ProgramError::InsufficientFunds);
    }
    let insurance_amount = cfg.total_deposited / 2;
    let backing_amount = cfg.total_deposited.saturating_sub(insurance_amount);
    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];

    // Configure the engine for full principal recoverability before funding it.
    // deposits_only=1 + max_bps=10000 + cooldown=0 lets the genesis program
    // withdraw up to the deposited insurance principal (never market profits,
    // which stay in the fund) with no rate limit — the engine side of allowing
    // genesis depositors to exit. The policy must be set *before* the top-up
    // because the engine only grows the deposits-only withdraw cap
    // (`insurance_withdraw_deposit_remaining`) on top-ups made while the policy
    // is already deposits-only.
    {
        let mut policy_ix_data = alloc::vec::Vec::with_capacity(12);
        policy_ix_data.push(PERC_IX_UPDATE_INSURANCE_POLICY);
        policy_ix_data.extend_from_slice(&GENESIS_INSURANCE_WITHDRAW_MAX_BPS.to_le_bytes());
        policy_ix_data.push(1); // deposits_only
        policy_ix_data.extend_from_slice(&0u64.to_le_bytes()); // cooldown_slots
        let ix = solana_program::instruction::Instruction {
            program_id: *percolator_program.key,
            accounts: alloc::vec![
                solana_program::instruction::AccountMeta::new_readonly(*market_admin.key, true),
                solana_program::instruction::AccountMeta::new(*market_slab.key, false),
            ],
            data: policy_ix_data,
        };
        invoke_signed(
            &ix,
            &[
                market_admin.clone(),
                market_slab.clone(),
                percolator_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }

    if insurance_amount > 0 {
        let mut insurance_ix_data = alloc::vec::Vec::with_capacity(17);
        insurance_ix_data.push(PERC_IX_TOP_UP_INSURANCE);
        insurance_ix_data.extend_from_slice(&(insurance_amount as u128).to_le_bytes());
        let ix = solana_program::instruction::Instruction {
            program_id: *percolator_program.key,
            accounts: alloc::vec![
                solana_program::instruction::AccountMeta::new_readonly(*market_admin.key, true),
                solana_program::instruction::AccountMeta::new(*market_slab.key, false),
                solana_program::instruction::AccountMeta::new(*genesis_vault.key, false),
                solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
                solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: insurance_ix_data,
        };
        invoke_signed(
            &ix,
            &[
                market_admin.clone(),
                market_slab.clone(),
                genesis_vault.clone(),
                percolator_vault.clone(),
                token_program.clone(),
                percolator_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }
    if backing_amount > 0 {
        let mut backing_ix_data = alloc::vec::Vec::with_capacity(27);
        backing_ix_data.push(PERC_IX_TOP_UP_BACKING_BUCKET);
        backing_ix_data.push(backing_domain);
        backing_ix_data.extend_from_slice(&(backing_amount as u128).to_le_bytes());
        backing_ix_data.extend_from_slice(&backing_expiry_slot.to_le_bytes());
        let ix = solana_program::instruction::Instruction {
            program_id: *percolator_program.key,
            accounts: alloc::vec![
                solana_program::instruction::AccountMeta::new_readonly(*market_admin.key, true),
                solana_program::instruction::AccountMeta::new(*market_slab.key, false),
                solana_program::instruction::AccountMeta::new(*genesis_vault.key, false),
                solana_program::instruction::AccountMeta::new(*percolator_vault.key, false),
                solana_program::instruction::AccountMeta::new_readonly(*token_program.key, false),
            ],
            data: backing_ix_data,
        };
        invoke_signed(
            &ix,
            &[
                market_admin.clone(),
                market_slab.clone(),
                genesis_vault.clone(),
                percolator_vault.clone(),
                token_program.clone(),
                percolator_program.clone(),
            ],
            &[&signer_seeds],
        )?;
    }
    cfg.kicked = 1;
    let mut cfg_data = genesis_cfg_account.try_borrow_mut_data()?;
    cfg.serialize(&mut cfg_data);
    Ok(())
}

// init_genesis_squads accounts:
//   [0] payer (signer, writable) — Squads multisig creator + rent/fee payer
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] market_admin PDA — becomes the multisig config_authority + sole 1/1 member
//   [5] squads create_key PDA [b"genesis_squads", coin_mint] — signed by this program
//   [6] squads program
//   [7] squads program_config PDA
//   [8] squads treasury (writable)
//   [9] multisig account (writable, created by Squads)
//   [10] system_program
//
// No data. Creates the per-coin controlled 1/1 multisig with a 48h timelock; its
// config_authority is this program's market_admin PDA until genesis handover.
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
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let create_key = next_account_info(iter)?;
    let squads_program = next_account_info(iter)?;
    let program_config = next_account_info(iter)?;
    let treasury = next_account_info(iter)?;
    let multisig = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if *squads_program.key != squads::PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    // Futarchy gate: governance authority must match CoinConfig.authority.
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;

    // create_key is a program PDA; the multisig address is derived from it.
    let (expected_create_key, ck_bump) =
        Pubkey::find_program_address(&squads::create_key_seeds(coin_mint.key), program_id);
    if *create_key.key != expected_create_key {
        msg!("genesis squads create-key PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }
    if *multisig.key != squads::multisig_address(create_key.key) {
        msg!("genesis squads multisig PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }

    // Encode MultisigCreateArgsV2: controlled 1/1 + 48h, config_authority =
    // market_admin PDA, single full-permission member = market_admin PDA.
    let mut ix_data = alloc::vec::Vec::with_capacity(96);
    ix_data.extend_from_slice(&squads::IX_MULTISIG_CREATE_V2);
    ix_data.push(1); // config_authority: Option = Some
    ix_data.extend_from_slice(market_admin.key.as_ref());
    ix_data.extend_from_slice(&1u16.to_le_bytes()); // threshold
    ix_data.extend_from_slice(&1u32.to_le_bytes()); // members.len()
    ix_data.extend_from_slice(market_admin.key.as_ref());
    ix_data.push(squads::PERM_ALL);
    ix_data.extend_from_slice(&squads::TIMELOCK_48H_SECS.to_le_bytes());
    ix_data.push(0); // rent_collector: Option = None
    ix_data.push(0); // memo: Option = None

    let ix = solana_program::instruction::Instruction {
        program_id: squads::PROGRAM_ID,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new_readonly(*program_config.key, false),
            solana_program::instruction::AccountMeta::new(*treasury.key, false),
            solana_program::instruction::AccountMeta::new(*multisig.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*create_key.key, true),
            solana_program::instruction::AccountMeta::new(*payer.key, true),
            solana_program::instruction::AccountMeta::new_readonly(*system_program.key, false),
        ],
        data: ix_data,
    };
    let ck_bump_bytes = [ck_bump];
    let create_key_signer: [&[u8]; 3] =
        [b"genesis_squads", coin_mint.key.as_ref(), &ck_bump_bytes];
    invoke_signed(
        &ix,
        &[
            program_config.clone(),
            treasury.clone(),
            multisig.clone(),
            create_key.clone(),
            payer.clone(),
            system_program.clone(),
            squads_program.clone(),
        ],
        &[&create_key_signer],
    )
}

// handover_genesis_squads accounts:
//   [0] payer (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] genesis_config PDA — must be finalized
//   [5] market_admin PDA — current config_authority; signed by this program
//   [6] squads program
//   [7] multisig account (writable)
//   [8] new config_authority (the winning genesis DAO)
//
// No data. Rotates the multisig config_authority from this program's
// market_admin PDA to the winning DAO. Percolator authority is never touched.
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
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let squads_program = next_account_info(iter)?;
    let multisig = next_account_info(iter)?;
    let new_authority = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *squads_program.key != squads::PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    validate_governance_authority(authority, coin_mint.key, program_id)?;
    let coin_cfg = load_coin_config(coin_cfg_account, coin_mint.key, program_id)?;
    if *authority.key != coin_cfg.authority {
        msg!("Signer does not match CoinConfig authority");
        return Err(ProgramError::MissingRequiredSignature);
    }
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_finalized() {
        msg!("genesis must be finalized before squads handover");
        return Err(ProgramError::InvalidInstructionData);
    }
    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;

    // Re-derive the multisig from this program's create-key PDA.
    let (create_key, _) =
        Pubkey::find_program_address(&squads::create_key_seeds(coin_mint.key), program_id);
    if *multisig.key != squads::multisig_address(&create_key) {
        msg!("genesis squads multisig PDA mismatch");
        return Err(ProgramError::InvalidSeeds);
    }

    // Rotate config_authority -> winning DAO. Only the current config_authority
    // (market_admin PDA) signs; the incoming authority is just an argument.
    let mut ix_data = alloc::vec::Vec::with_capacity(41);
    ix_data.extend_from_slice(&squads::IX_SET_CONFIG_AUTHORITY);
    ix_data.extend_from_slice(new_authority.key.as_ref());
    ix_data.push(0); // memo: Option = None

    // MultisigConfig accounts: multisig(mut), config_authority(signer),
    // rent_payer(Option=None), system_program(Option=None). None optionals are
    // passed as the Squads program id (Anchor's sentinel for an absent account).
    let ix = solana_program::instruction::Instruction {
        program_id: squads::PROGRAM_ID,
        accounts: alloc::vec![
            solana_program::instruction::AccountMeta::new(*multisig.key, false),
            solana_program::instruction::AccountMeta::new_readonly(*market_admin.key, true),
            solana_program::instruction::AccountMeta::new_readonly(squads::PROGRAM_ID, false),
            solana_program::instruction::AccountMeta::new_readonly(squads::PROGRAM_ID, false),
        ],
        data: ix_data,
    };
    let admin_bump_bytes = [admin_bump];
    let admin_signer: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &admin_bump_bytes,
    ];
    invoke_signed(
        &ix,
        &[multisig.clone(), market_admin.clone(), squads_program.clone()],
        &[&admin_signer],
    )
}

// recover_genesis_market accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] genesis_config PDA
//   [5] market_admin PDA
//   [6] market_slab (writable)
//   [7] genesis_vault (writable; destination, owned by market_admin PDA)
//   [8] percolator_vault (writable)
//   [9] percolator_vault_pda
//   [10] percolator_program
//   [11] token_program
//   [12] optional percolator ledger account (required for backing earnings)
//
// Data: recovery_kind (u8), domain (u8), amount (u64)

fn process_recover_genesis_market<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let genesis_cfg_account = next_account_info(iter)?;
    let market_admin = next_account_info(iter)?;
    let market_slab = next_account_info(iter)?;
    let genesis_vault = next_account_info(iter)?;
    let percolator_vault = next_account_info(iter)?;
    let percolator_vault_pda = next_account_info(iter)?;
    let percolator_program = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let recovery_kind = read_u8(data)?;
    let domain = read_u8(data)?;
    let amount = read_u64(data)?;
    if !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_token_program(token_program)?;
    verify_percolator_program(percolator_program)?;
    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;

    let admin_bump = verify_market_admin_pda(market_admin, coin_mint.key, program_id)?;
    let cfg = verify_genesis_config_pda(genesis_cfg_account, coin_mint.key, program_id)?;
    if !cfg.is_kicked() || cfg.is_finalized() {
        msg!("genesis market recovery requires kicked, unfinalized genesis");
        return Err(ProgramError::InvalidInstructionData);
    }
    verify_genesis_vault(genesis_vault, &cfg, market_admin.key, program_id)?;
    let percolator_cfg = load_percolator_market_config(market_slab, &cfg.base_mint)?;
    if percolator_cfg.admin != market_admin.key.to_bytes()
        || percolator_cfg.insurance_authority != market_admin.key.to_bytes()
        || percolator_cfg.insurance_operator != market_admin.key.to_bytes()
        || percolator_cfg.backing_bucket_authority != market_admin.key.to_bytes()
    {
        msg!("genesis recovery market must be controlled by the COIN market-admin PDA");
        return Err(ProgramError::InvalidAccountData);
    }
    validate_percolator_vault_accounts(
        market_slab,
        percolator_vault,
        percolator_vault_pda,
        &cfg.base_mint,
    )?;

    let ix_data = genesis_recovery_ix_data(recovery_kind, domain, amount)?;
    let bump_bytes = [admin_bump];
    let signer_seeds: [&[u8]; 3] = [
        b"percolator_market_admin",
        coin_mint.key.as_ref(),
        &bump_bytes,
    ];

    let mut metas = alloc::vec::Vec::new();
    let mut cpi_accounts = alloc::vec::Vec::new();
    metas.push(solana_program::instruction::AccountMeta::new_readonly(
        *market_admin.key,
        true,
    ));
    metas.push(solana_program::instruction::AccountMeta::new(
        *market_slab.key,
        false,
    ));
    cpi_accounts.push(market_admin.clone());
    cpi_accounts.push(market_slab.clone());

    let ledger_account = if recovery_kind == GENESIS_RECOVER_BACKING_EARNINGS {
        let ledger_account = iter.next().ok_or(ProgramError::NotEnoughAccountKeys)?;
        if iter.next().is_some() {
            return Err(ProgramError::InvalidInstructionData);
        }
        if ledger_account.owner != &percolator_abi::id() {
            return Err(ProgramError::IllegalOwner);
        }
        Some(ledger_account)
    } else {
        if iter.next().is_some() {
            msg!("ledger account is only accepted for backing earnings recovery");
            return Err(ProgramError::InvalidInstructionData);
        }
        None
    };

    if let Some(ledger_account) = ledger_account {
        metas.push(solana_program::instruction::AccountMeta::new(
            *ledger_account.key,
            false,
        ));
        cpi_accounts.push(ledger_account.clone());
    }

    metas.push(solana_program::instruction::AccountMeta::new(
        *genesis_vault.key,
        false,
    ));
    metas.push(solana_program::instruction::AccountMeta::new(
        *percolator_vault.key,
        false,
    ));
    metas.push(solana_program::instruction::AccountMeta::new_readonly(
        *percolator_vault_pda.key,
        false,
    ));
    metas.push(solana_program::instruction::AccountMeta::new_readonly(
        *token_program.key,
        false,
    ));
    cpi_accounts.push(genesis_vault.clone());
    cpi_accounts.push(percolator_vault.clone());
    cpi_accounts.push(percolator_vault_pda.clone());
    cpi_accounts.push(token_program.clone());

    cpi_accounts.push(percolator_program.clone());
    let ix = solana_program::instruction::Instruction {
        program_id: *percolator_program.key,
        accounts: metas,
        data: ix_data,
    };
    invoke_signed(&ix, &cpi_accounts, &[&signer_seeds])
}

// approve_builder accounts:
//   [0] payer/controller (signer)
//   [1] authority (signer, governance PDA)
//   [2] coin_mint
//   [3] coin_config PDA
//   [4] builder_program
//   [5] builder_approval PDA (writable, created or updated)
//   [6] system_program
//   [7] clock
//
// Data: code_hash ([u8;32]), terms_hash ([u8;32]), enabled (u8)

fn process_approve_builder<'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    data: &mut &[u8],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let authority = next_account_info(iter)?;
    let coin_mint = next_account_info(iter)?;
    let coin_cfg_account = next_account_info(iter)?;
    let builder_program = next_account_info(iter)?;
    let approval_account = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let clock_info = next_account_info(iter)?;

    let code_hash = read_bytes32(data)?;
    let terms_hash = read_bytes32(data)?;
    let enabled = read_u8(data)?;
    if enabled > 1 || !data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    if *system_program.key != solana_program::system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    require_genesis_governance(program_id, payer, authority, coin_mint, coin_cfg_account)?;
    if !builder_program.executable {
        msg!("builder approval target must be an executable program account");
        return Err(ProgramError::InvalidAccountData);
    }
    if builder_program.owner != &solana_program::bpf_loader::ID
        && builder_program.owner != &solana_program::bpf_loader_upgradeable::ID
    {
        msg!("builder approval target must be owned by a BPF loader");
        return Err(ProgramError::IllegalOwner);
    }
    let seeds = builder_approval_seeds(coin_mint.key, builder_program.key, &code_hash);
    let expected = Pubkey::find_program_address(&seeds, program_id).0;
    if *approval_account.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if approval_account.data_len() == 0 || approval_account.lamports() == 0 {
        create_pda_account(
            payer,
            approval_account,
            system_program,
            program_id,
            &seeds,
            BUILDER_APPROVAL_SIZE,
        )?;
    } else if approval_account.owner != program_id {
        return Err(ProgramError::IllegalOwner);
    } else {
        let approval_data = approval_account.try_borrow_data()?;
        let existing = BuilderApproval::deserialize(&approval_data)?;
        if existing.coin_mint != *coin_mint.key
            || existing.builder_program != *builder_program.key
            || existing.code_hash != code_hash
        {
            return Err(ProgramError::InvalidAccountData);
        }
        drop(approval_data);
    }
    let clock = Clock::from_account_info(clock_info)?;
    let approval = BuilderApproval {
        coin_mint: *coin_mint.key,
        builder_program: *builder_program.key,
        code_hash,
        terms_hash,
        approved_slot: clock.slot,
        enabled,
    };
    let mut approval_data = approval_account.try_borrow_mut_data()?;
    approval.serialize(&mut approval_data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_config_round_trips_fixed_layout() {
        let cfg = GenesisConfig {
            coin_mint: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            token_vault: Pubkey::new_unique(),
            total_deposited: 101,
            total_withdrawn: 1,
            reward_supply: 1_000_000,
            minted_supply: 250_000,
            finalized: 1,
            kicked: 1,
            total_voted_principal: 64,
            total_cast_weight: 320,
        };

        let mut bytes = [0u8; GENESIS_CFG_SIZE];
        cfg.serialize(&mut bytes);
        let decoded = GenesisConfig::deserialize(&bytes).unwrap();

        assert_eq!(decoded.coin_mint, cfg.coin_mint);
        assert_eq!(decoded.base_mint, cfg.base_mint);
        assert_eq!(decoded.token_vault, cfg.token_vault);
        assert_eq!(decoded.total_deposited, cfg.total_deposited);
        assert_eq!(decoded.total_withdrawn, cfg.total_withdrawn);
        assert_eq!(decoded.reward_supply, cfg.reward_supply);
        assert_eq!(decoded.minted_supply, cfg.minted_supply);
        assert!(decoded.is_finalized());
        assert!(decoded.is_kicked());
        assert_eq!(decoded.total_voted_principal, 64);
        assert_eq!(decoded.total_cast_weight, 320);
        assert_eq!(decoded.outstanding_principal(), 100);
    }

    #[test]
    fn genesis_withdrawal_returns_up_to_principal_pro_rata() {
        assert_eq!(genesis_recoverable_principal(10, 100, 100).unwrap(), 10);
        assert_eq!(genesis_recoverable_principal(10, 50, 100).unwrap(), 5);
        assert_eq!(genesis_recoverable_principal(3, 2, 10).unwrap(), 0);
        assert!(genesis_recoverable_principal(1, 0, 0).is_err());
    }

    #[test]
    fn genesis_recovery_builds_only_supported_percolator_withdrawals() {
        let insurance = genesis_recovery_ix_data(GENESIS_RECOVER_INSURANCE_LIMITED, 7, 5).unwrap();
        assert_eq!(insurance[0], PERC_IX_WITHDRAW_INSURANCE_LIMITED);
        assert_eq!(insurance.len(), 17);
        assert_eq!(u128::from_le_bytes(insurance[1..17].try_into().unwrap()), 5);

        let backing = genesis_recovery_ix_data(GENESIS_RECOVER_BACKING, 3, 9).unwrap();
        assert_eq!(backing[0], PERC_IX_WITHDRAW_BACKING_BUCKET);
        assert_eq!(backing[1], 3);
        assert_eq!(u128::from_le_bytes(backing[2..18].try_into().unwrap()), 9);

        let terminal = genesis_recovery_ix_data(GENESIS_RECOVER_INSURANCE_TERMINAL, 0, 11).unwrap();
        assert_eq!(terminal[0], PERC_IX_WITHDRAW_INSURANCE);
        assert_eq!(terminal.len(), 17);

        assert!(genesis_recovery_ix_data(99, 0, 1).is_err());
        assert!(genesis_recovery_ix_data(GENESIS_RECOVER_BACKING, 0, 0).is_err());
    }

    #[test]
    fn futarchy_admin_proxy_is_lifecycle_scoped() {
        assert!(!percolator_admin_tag_allowed(PERC_IX_INIT_MARKET));
        assert!(percolator_admin_tag_allowed(
            PERC_IX_UPDATE_MARKET_INIT_FEE_POLICY
        ));
        assert!(percolator_admin_tag_allowed(PERC_IX_UPDATE_ASSET_LIFECYCLE));
        assert!(percolator_admin_tag_allowed(PERC_IX_RESOLVE_MARKET));
        assert!(percolator_admin_tag_allowed(PERC_IX_CLOSE_SLAB));
        assert!(!percolator_admin_tag_allowed(PERC_IX_UPDATE_AUTHORITY));
        assert!(!percolator_admin_tag_allowed(PERC_IX_TOP_UP_INSURANCE));
        assert!(!percolator_admin_tag_allowed(
            PERC_IX_WITHDRAW_BACKING_BUCKET
        ));
    }
}
