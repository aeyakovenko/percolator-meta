use crate::{
    defaults::{
        DEFAULT_INITIAL_MARGIN_BPS, DEFAULT_MAX_ACCRUAL_DT_SLOTS,
        DEFAULT_PERMISSIONLESS_MARKET_CREATION_FEE_USDC_ATOMS,
    },
    Amount,
};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

pub const MARKET_0_ASSET_INDEX: u16 = 0;
pub const ASSET_AUTH_ADMIN: u8 = 0;
pub const ASSET_AUTH_INSURANCE: u8 = 1;
pub const ASSET_AUTH_INSURANCE_OPERATOR: u8 = 2;
pub const ASSET_AUTH_BACKING_BUCKET: u8 = 3;
pub const ASSET_AUTH_ORACLE: u8 = 4;

pub const MARKET_0_FEE_SHARE_BPS: u16 = 2_000;
pub const MAX_PROTOCOL_FEE_ABS: Amount = 1_000_000_000_000_000_000_000_000_000_000_000_000;
pub const HL_FEE_MILLI_BPS_PER_BPS: u64 = 1_000;
pub const HL_BASE_PERP_TAKER_FEE_MILLI_BPS: u64 = 4_500;
pub const HL_BASE_PERP_MAKER_FEE_MILLI_BPS: u64 = 1_500;
pub const HL_BASE_PERP_SYMMETRIC_FEE_MILLI_BPS: u64 =
    (HL_BASE_PERP_TAKER_FEE_MILLI_BPS + HL_BASE_PERP_MAKER_FEE_MILLI_BPS) / 2;
pub const HL_POST_DISCOUNT_SYMMETRIC_FEE_MILLI_BPS: u64 = 2_500;
pub const HL_GENERAL_TRADE_FEE_BPS: u64 = 2;

pub const IX_INIT_MARKET: u8 = 0;
pub const IX_TOP_UP_INSURANCE: u8 = 9;
pub const IX_TOP_UP_BACKING_BUCKET: u8 = 24;
pub const IX_UPDATE_AUTHORITY: u8 = 32;
pub const IX_UPDATE_INSURANCE_POLICY: u8 = 33;
pub const IX_WITHDRAW_INSURANCE: u8 = 41;
pub const IX_UPDATE_BACKING_FEE_POLICY: u8 = 51;
pub const IX_WITHDRAW_BACKING_BUCKET: u8 = 50;
pub const IX_SYNC_BACKING_DOMAIN_LEDGER: u8 = 53;
pub const IX_SYNC_INSURANCE_LEDGER: u8 = 54;
pub const IX_UPDATE_TRADE_FEE_POLICY: u8 = 55;
pub const IX_UPDATE_FEE_REDIRECT_POLICY: u8 = 58;
pub const IX_UPDATE_MARKET_INIT_FEE_POLICY: u8 = 59;
pub const IX_UPDATE_ASSET_AUTHORITY: u8 = 65;
pub const IX_WITHDRAW_INSURANCE_DOMAIN: u8 = 57;

pub const DEFAULT_INIT_MARKET_SLOTS: u16 = 1;
pub const DEFAULT_H_MIN: u64 = 0;
pub const DEFAULT_H_MAX: u64 = 6_480_000;
pub const DEFAULT_MAINTENANCE_MARGIN_BPS: u64 = DEFAULT_INITIAL_MARGIN_BPS;
pub const DEFAULT_MAX_PRICE_MOVE_BPS_PER_SLOT: u64 = 24;
pub const DEFAULT_MAX_TRADING_FEE_BPS: u64 = 100;
pub const DEFAULT_MAX_ABS_FUNDING_E9_PER_SLOT: u64 = 1_000;
pub const DEFAULT_MIN_FUNDING_LIFETIME_SLOTS: u64 = 10_000_000;
pub const DEFAULT_LIQUIDATION_FEE_BPS: u64 = 5;
pub const DEFAULT_MAX_ACCOUNT_B_SETTLEMENT_CHUNKS: u64 = 32;
pub const DEFAULT_MAX_BANKRUPT_CLOSE_CHUNKS: u64 = 32;
pub const DEFAULT_MAX_BANKRUPT_CLOSE_LIFETIME_SLOTS: u64 = 1_000;
pub const DEFAULT_PUBLIC_B_CHUNK_ATOMS: Amount = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlPostDiscountFees {
    pub taker_milli_bps: u64,
    pub maker_milli_bps: u64,
}

impl HlPostDiscountFees {
    pub const fn base_perps() -> Self {
        Self {
            taker_milli_bps: HL_BASE_PERP_TAKER_FEE_MILLI_BPS,
            maker_milli_bps: HL_BASE_PERP_MAKER_FEE_MILLI_BPS,
        }
    }

    pub const fn symmetric_post_discount() -> Self {
        Self {
            taker_milli_bps: HL_POST_DISCOUNT_SYMMETRIC_FEE_MILLI_BPS,
            maker_milli_bps: HL_POST_DISCOUNT_SYMMETRIC_FEE_MILLI_BPS,
        }
    }

    pub fn symmetric_milli_bps(self) -> Option<u64> {
        self.taker_milli_bps
            .checked_add(self.maker_milli_bps)
            .map(|sum| sum / 2)
    }

    pub fn symmetric_bps_ceiling(self) -> Option<u64> {
        self.symmetric_milli_bps()?
            .checked_add(HL_FEE_MILLI_BPS_PER_BPS - 1)
            .map(|fee| fee / HL_FEE_MILLI_BPS_PER_BPS)
    }

    pub fn symmetric_bps_floor(self) -> Option<u64> {
        self.symmetric_milli_bps()
            .map(|fee| fee / HL_FEE_MILLI_BPS_PER_BPS)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitMarketConfig {
    pub max_portfolio_assets: u16,
    pub h_min: u64,
    pub h_max: u64,
    pub initial_price: u64,
    pub min_nonzero_mm_req: Amount,
    pub min_nonzero_im_req: Amount,
    pub maintenance_margin_bps: u64,
    pub initial_margin_bps: u64,
    pub max_trading_fee_bps: u64,
    pub trade_fee_base_bps: u64,
    pub liquidation_fee_bps: u64,
    pub liquidation_fee_cap: Amount,
    pub min_liquidation_abs: Amount,
    pub max_price_move_bps_per_slot: u64,
    pub max_accrual_dt_slots: u64,
    pub max_abs_funding_e9_per_slot: u64,
    pub min_funding_lifetime_slots: u64,
    pub max_account_b_settlement_chunks: u64,
    pub max_bankrupt_close_chunks: u64,
    pub max_bankrupt_close_lifetime_slots: u64,
    pub public_b_chunk_atoms: Amount,
    pub maintenance_fee_per_slot: Amount,
}

impl InitMarketConfig {
    pub fn default_permissionless(initial_price: u64) -> Self {
        Self {
            max_portfolio_assets: DEFAULT_INIT_MARKET_SLOTS,
            h_min: DEFAULT_H_MIN,
            h_max: DEFAULT_H_MAX,
            initial_price,
            min_nonzero_mm_req: 599,
            min_nonzero_im_req: 600,
            maintenance_margin_bps: DEFAULT_MAINTENANCE_MARGIN_BPS,
            initial_margin_bps: DEFAULT_INITIAL_MARGIN_BPS,
            max_trading_fee_bps: DEFAULT_MAX_TRADING_FEE_BPS,
            trade_fee_base_bps: HL_GENERAL_TRADE_FEE_BPS,
            liquidation_fee_bps: DEFAULT_LIQUIDATION_FEE_BPS,
            liquidation_fee_cap: MAX_PROTOCOL_FEE_ABS,
            min_liquidation_abs: 0,
            max_price_move_bps_per_slot: DEFAULT_MAX_PRICE_MOVE_BPS_PER_SLOT,
            max_accrual_dt_slots: DEFAULT_MAX_ACCRUAL_DT_SLOTS,
            max_abs_funding_e9_per_slot: DEFAULT_MAX_ABS_FUNDING_E9_PER_SLOT,
            min_funding_lifetime_slots: DEFAULT_MIN_FUNDING_LIFETIME_SLOTS,
            max_account_b_settlement_chunks: DEFAULT_MAX_ACCOUNT_B_SETTLEMENT_CHUNKS,
            max_bankrupt_close_chunks: DEFAULT_MAX_BANKRUPT_CLOSE_CHUNKS,
            max_bankrupt_close_lifetime_slots: DEFAULT_MAX_BANKRUPT_CLOSE_LIFETIME_SLOTS,
            public_b_chunk_atoms: DEFAULT_PUBLIC_B_CHUNK_ATOMS,
            maintenance_fee_per_slot: 0,
        }
    }
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u128(out: &mut Vec<u8>, value: Amount) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn encode_top_up_insurance(amount: Amount) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.push(IX_TOP_UP_INSURANCE);
    push_u128(&mut out, amount);
    out
}

pub fn encode_init_market(config: InitMarketConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 2 + 8 * 15 + 16 * 6);
    out.push(IX_INIT_MARKET);
    push_u16(&mut out, config.max_portfolio_assets);
    push_u64(&mut out, config.h_min);
    push_u64(&mut out, config.h_max);
    push_u64(&mut out, config.initial_price);
    push_u128(&mut out, config.min_nonzero_mm_req);
    push_u128(&mut out, config.min_nonzero_im_req);
    push_u64(&mut out, config.maintenance_margin_bps);
    push_u64(&mut out, config.initial_margin_bps);
    push_u64(&mut out, config.max_trading_fee_bps);
    push_u64(&mut out, config.trade_fee_base_bps);
    push_u64(&mut out, config.liquidation_fee_bps);
    push_u128(&mut out, config.liquidation_fee_cap);
    push_u128(&mut out, config.min_liquidation_abs);
    push_u64(&mut out, config.max_price_move_bps_per_slot);
    push_u64(&mut out, config.max_accrual_dt_slots);
    push_u64(&mut out, config.max_abs_funding_e9_per_slot);
    push_u64(&mut out, config.min_funding_lifetime_slots);
    push_u64(&mut out, config.max_account_b_settlement_chunks);
    push_u64(&mut out, config.max_bankrupt_close_chunks);
    push_u64(&mut out, config.max_bankrupt_close_lifetime_slots);
    push_u128(&mut out, config.public_b_chunk_atoms);
    push_u128(&mut out, config.maintenance_fee_per_slot);
    out
}

pub fn encode_top_up_backing_bucket(domain: u8, amount: Amount, expiry_slot: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(26);
    out.push(IX_TOP_UP_BACKING_BUCKET);
    out.push(domain);
    push_u128(&mut out, amount);
    push_u64(&mut out, expiry_slot);
    out
}

pub fn encode_update_authority(new_pubkey: Pubkey) -> Vec<u8> {
    let mut out = Vec::with_capacity(33);
    out.push(IX_UPDATE_AUTHORITY);
    out.extend_from_slice(new_pubkey.as_ref());
    out
}

pub fn encode_update_asset_authority(asset_index: u16, kind: u8, new_pubkey: Pubkey) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    out.push(IX_UPDATE_ASSET_AUTHORITY);
    push_u16(&mut out, asset_index);
    out.push(kind);
    out.extend_from_slice(new_pubkey.as_ref());
    out
}

pub fn encode_update_insurance_policy(
    max_bps: u16,
    deposits_only: bool,
    cooldown_slots: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.push(IX_UPDATE_INSURANCE_POLICY);
    push_u16(&mut out, max_bps);
    out.push(u8::from(deposits_only));
    push_u64(&mut out, cooldown_slots);
    out
}

pub fn encode_update_backing_fee_policy(
    domain: u8,
    fee_bps: u16,
    insurance_share_bps: u16,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(6);
    out.push(IX_UPDATE_BACKING_FEE_POLICY);
    out.push(domain);
    push_u16(&mut out, fee_bps);
    push_u16(&mut out, insurance_share_bps);
    out
}

pub fn encode_update_fee_redirect_policy(redirect_bps: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    out.push(IX_UPDATE_FEE_REDIRECT_POLICY);
    push_u16(&mut out, redirect_bps);
    out
}

pub fn encode_update_trade_fee_policy(trade_fee_base_bps: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(IX_UPDATE_TRADE_FEE_POLICY);
    push_u64(&mut out, trade_fee_base_bps);
    out
}

pub fn encode_update_market_init_fee_policy(min_init_fee: Amount) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.push(IX_UPDATE_MARKET_INIT_FEE_POLICY);
    push_u128(&mut out, min_init_fee);
    out
}

pub fn encode_withdraw_backing_bucket(domain: u8, amount: Amount) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    out.push(IX_WITHDRAW_BACKING_BUCKET);
    out.push(domain);
    push_u128(&mut out, amount);
    out
}

pub fn encode_withdraw_insurance(amount: Amount) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.push(IX_WITHDRAW_INSURANCE);
    push_u128(&mut out, amount);
    out
}

pub fn encode_withdraw_insurance_domain(domain: u8, amount: Amount) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    out.push(IX_WITHDRAW_INSURANCE_DOMAIN);
    out.push(domain);
    push_u128(&mut out, amount);
    out
}

pub fn encode_sync_backing_domain_ledger(domain: u8) -> Vec<u8> {
    vec![IX_SYNC_BACKING_DOMAIN_LEDGER, domain]
}

pub fn encode_sync_insurance_ledger() -> Vec<u8> {
    vec![IX_SYNC_INSURANCE_LEDGER]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PercolatorAccounts {
    pub program: Pubkey,
    pub market: Pubkey,
    pub authority: Pubkey,
    pub authority_token: Pubkey,
    pub vault_token: Pubkey,
    pub vault_authority: Pubkey,
    pub token_program: Pubkey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitMarketAccounts {
    pub program: Pubkey,
    pub admin: Pubkey,
    pub market: Pubkey,
    pub collateral_mint: Pubkey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WithdrawInsuranceAccounts {
    pub program: Pubkey,
    pub authority: Pubkey,
    pub market: Pubkey,
    pub destination_token: Pubkey,
    pub vault_token: Pubkey,
    pub vault_authority: Pubkey,
    pub token_program: Pubkey,
}

pub fn init_market_ix(accounts: InitMarketAccounts, config: InitMarketConfig) -> Instruction {
    Instruction {
        program_id: accounts.program,
        accounts: vec![
            AccountMeta::new_readonly(accounts.admin, true),
            AccountMeta::new(accounts.market, false),
            AccountMeta::new_readonly(accounts.collateral_mint, false),
        ],
        data: encode_init_market(config),
    }
}

pub fn top_up_insurance_ix(
    accounts: PercolatorAccounts,
    insurance_ledger: Option<Pubkey>,
    amount: Amount,
) -> Instruction {
    let mut metas = vec![
        AccountMeta::new_readonly(accounts.authority, true),
        AccountMeta::new(accounts.market, false),
        AccountMeta::new(accounts.authority_token, false),
        AccountMeta::new(accounts.vault_token, false),
        AccountMeta::new_readonly(accounts.token_program, false),
    ];
    if let Some(ledger) = insurance_ledger {
        metas.push(AccountMeta::new(ledger, false));
    }
    Instruction {
        program_id: accounts.program,
        accounts: metas,
        data: encode_top_up_insurance(amount),
    }
}

pub fn top_up_backing_bucket_ix(
    accounts: PercolatorAccounts,
    backing_ledger: Option<Pubkey>,
    domain: u8,
    amount: Amount,
    expiry_slot: u64,
) -> Instruction {
    let mut metas = vec![
        AccountMeta::new_readonly(accounts.authority, true),
        AccountMeta::new(accounts.market, false),
        AccountMeta::new(accounts.authority_token, false),
        AccountMeta::new(accounts.vault_token, false),
        AccountMeta::new_readonly(accounts.token_program, false),
    ];
    if let Some(ledger) = backing_ledger {
        metas.push(AccountMeta::new(ledger, false));
    }
    Instruction {
        program_id: accounts.program,
        accounts: metas,
        data: encode_top_up_backing_bucket(domain, amount, expiry_slot),
    }
}

pub fn withdraw_insurance_ix(
    accounts: WithdrawInsuranceAccounts,
    insurance_ledger: Option<Pubkey>,
    amount: Amount,
) -> Instruction {
    let mut metas = vec![
        AccountMeta::new_readonly(accounts.authority, true),
        AccountMeta::new(accounts.market, false),
        AccountMeta::new(accounts.destination_token, false),
        AccountMeta::new(accounts.vault_token, false),
        AccountMeta::new_readonly(accounts.vault_authority, false),
        AccountMeta::new_readonly(accounts.token_program, false),
    ];
    if let Some(ledger) = insurance_ledger {
        metas.push(AccountMeta::new(ledger, false));
    }
    Instruction {
        program_id: accounts.program,
        accounts: metas,
        data: encode_withdraw_insurance(amount),
    }
}

pub fn withdraw_insurance_domain_ix(
    accounts: WithdrawInsuranceAccounts,
    insurance_ledger: Option<Pubkey>,
    domain: u8,
    amount: Amount,
) -> Instruction {
    let mut metas = vec![
        AccountMeta::new_readonly(accounts.authority, true),
        AccountMeta::new(accounts.market, false),
        AccountMeta::new(accounts.destination_token, false),
        AccountMeta::new(accounts.vault_token, false),
        AccountMeta::new_readonly(accounts.vault_authority, false),
        AccountMeta::new_readonly(accounts.token_program, false),
    ];
    if let Some(ledger) = insurance_ledger {
        metas.push(AccountMeta::new(ledger, false));
    }
    Instruction {
        program_id: accounts.program,
        accounts: metas,
        data: encode_withdraw_insurance_domain(domain, amount),
    }
}

pub fn update_authority_ix(
    program: Pubkey,
    current_authority: Pubkey,
    new_authority: Pubkey,
    market: Pubkey,
) -> Instruction {
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(current_authority, true),
            AccountMeta::new_readonly(new_authority, true),
            AccountMeta::new(market, false),
        ],
        data: encode_update_authority(new_authority),
    }
}

pub fn update_asset_authority_ix(
    program: Pubkey,
    current_authority: Pubkey,
    new_authority: Pubkey,
    market: Pubkey,
    asset_index: u16,
    kind: u8,
) -> Instruction {
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(current_authority, true),
            AccountMeta::new_readonly(new_authority, true),
            AccountMeta::new(market, false),
        ],
        data: encode_update_asset_authority(asset_index, kind, new_authority),
    }
}

pub fn update_insurance_policy_ix(
    program: Pubkey,
    admin: Pubkey,
    market: Pubkey,
    max_bps: u16,
    deposits_only: bool,
    cooldown_slots: u64,
) -> Instruction {
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(admin, true),
            AccountMeta::new(market, false),
        ],
        data: encode_update_insurance_policy(max_bps, deposits_only, cooldown_slots),
    }
}

pub fn update_backing_fee_policy_ix(
    program: Pubkey,
    insurance_authority: Pubkey,
    market: Pubkey,
    domain: u8,
    fee_bps: u16,
    insurance_share_bps: u16,
) -> Instruction {
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(insurance_authority, true),
            AccountMeta::new(market, false),
        ],
        data: encode_update_backing_fee_policy(domain, fee_bps, insurance_share_bps),
    }
}

pub fn update_fee_redirect_policy_ix(
    program: Pubkey,
    admin: Pubkey,
    market: Pubkey,
    redirect_bps: u16,
) -> Instruction {
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(admin, true),
            AccountMeta::new(market, false),
        ],
        data: encode_update_fee_redirect_policy(redirect_bps),
    }
}

pub fn update_trade_fee_policy_ix(
    program: Pubkey,
    insurance_authority: Pubkey,
    market: Pubkey,
    trade_fee_base_bps: u64,
) -> Instruction {
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(insurance_authority, true),
            AccountMeta::new(market, false),
        ],
        data: encode_update_trade_fee_policy(trade_fee_base_bps),
    }
}

pub fn update_market_init_fee_policy_ix(
    program: Pubkey,
    admin: Pubkey,
    market: Pubkey,
    min_init_fee: Amount,
) -> Instruction {
    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(admin, true),
            AccountMeta::new(market, false),
        ],
        data: encode_update_market_init_fee_policy(min_init_fee),
    }
}

pub fn default_hl_trade_fee_policy_ix(
    program: Pubkey,
    insurance_authority: Pubkey,
    market: Pubkey,
) -> Instruction {
    update_trade_fee_policy_ix(
        program,
        insurance_authority,
        market,
        HL_GENERAL_TRADE_FEE_BPS,
    )
}

pub fn hl_post_discount_symmetric_trade_fee_policy_ix(
    program: Pubkey,
    insurance_authority: Pubkey,
    market: Pubkey,
    fees: HlPostDiscountFees,
) -> Option<Instruction> {
    Some(update_trade_fee_policy_ix(
        program,
        insurance_authority,
        market,
        fees.symmetric_bps_floor()?,
    ))
}

pub fn default_permissionless_market_init_fee_ix(
    program: Pubkey,
    admin: Pubkey,
    market: Pubkey,
) -> Instruction {
    update_market_init_fee_policy_ix(
        program,
        admin,
        market,
        DEFAULT_PERMISSIONLESS_MARKET_CREATION_FEE_USDC_ATOMS,
    )
}

pub fn default_market_0_fee_capture_ixs(
    program: Pubkey,
    admin: Pubkey,
    market: Pubkey,
    market_0_insurance_authority: Pubkey,
    market_0_domain_backing_fee_bps: &[(u8, u16)],
) -> Vec<Instruction> {
    let mut instructions = Vec::with_capacity(1 + market_0_domain_backing_fee_bps.len());
    instructions.push(update_fee_redirect_policy_ix(
        program,
        admin,
        market,
        MARKET_0_FEE_SHARE_BPS,
    ));
    instructions.extend(
        market_0_domain_backing_fee_bps
            .iter()
            .map(|(domain, fee_bps)| {
                update_backing_fee_policy_ix(
                    program,
                    market_0_insurance_authority,
                    market,
                    *domain,
                    *fee_bps,
                    MARKET_0_FEE_SHARE_BPS,
                )
            }),
    );
    instructions
}
