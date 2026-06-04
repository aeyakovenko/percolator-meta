use crate::{
    percolator_v16::{
        update_asset_authority_ix, update_market_0_insurance_operator_by_asset_admin_ix,
        withdraw_insurance_domain_ix, WithdrawInsuranceAccounts, ASSET_AUTH_INSURANCE_OPERATOR,
        MARKET_0_ASSET_INDEX,
    },
    Amount, Slot,
};
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use std::{
    cmp::{min, Ordering},
    str::FromStr,
};
use thiserror::Error;

pub const MARKET_0_SURPLUS_BUY_BURN_BPS: u16 = 8_000;
pub const BPS_DENOMINATOR: u16 = 10_000;
pub const MARKET_0_SURPLUS_RETAIN_BPS: u16 = BPS_DENOMINATOR - MARKET_0_SURPLUS_BUY_BURN_BPS;
pub const TARGET_SLOTS_PER_SECOND: Slot = 2;
pub const TWAP_INTERVAL_SECONDS: Slot = 5 * 60;
pub const TWAP_INTERVAL_SLOTS: Slot = TWAP_INTERVAL_SECONDS * TARGET_SLOTS_PER_SECOND;
pub const TWAP_INTERVAL_COUNT: Slot = 150;
pub const MAX_TWAP_INTERVAL_COUNT: Slot = TWAP_INTERVAL_COUNT;
pub const TWAP_TOTAL_SLOTS: Slot = TWAP_INTERVAL_SLOTS * TWAP_INTERVAL_COUNT;
pub const TWAP_AUTHORITY_SEED: &[u8] = b"market-0-twap";
pub const BID_ESCROW_SEED: &[u8] = b"bid-escrow";
pub const MAX_TWAP_BIDS_PER_EXECUTION: usize = 64;
pub const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

pub fn associated_token_program_id() -> Pubkey {
    Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM_ID).expect("associated token program id is valid")
}

pub fn derive_associated_token_address(
    wallet: Pubkey,
    mint: Pubkey,
    token_program: Pubkey,
) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_token_program_id(),
    )
    .0
}

pub fn derive_bid_escrow_pda(bidder: Pubkey, twap_program: Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[BID_ESCROW_SEED, bidder.as_ref()], &twap_program).0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TwapAuthorityChain {
    pub metadao_futarchy: Pubkey,
    pub squads: Pubkey,
    pub twap_program: Pubkey,
    pub percolator_program: Pubkey,
    pub market: Pubkey,
    pub twap_pda: Pubkey,
    pub bump: u8,
}

impl TwapAuthorityChain {
    pub fn new(
        metadao_futarchy: Pubkey,
        squads: Pubkey,
        twap_program: Pubkey,
        percolator_program: Pubkey,
        market: Pubkey,
    ) -> Result<Self, SurplusError> {
        require_nonzero(metadao_futarchy)?;
        require_nonzero(squads)?;
        require_nonzero(twap_program)?;
        require_nonzero(percolator_program)?;
        require_nonzero(market)?;
        let (twap_pda, bump) = Pubkey::find_program_address(
            &[
                TWAP_AUTHORITY_SEED,
                market.as_ref(),
                percolator_program.as_ref(),
            ],
            &twap_program,
        );
        Ok(Self {
            metadao_futarchy,
            squads,
            twap_program,
            percolator_program,
            market,
            twap_pda,
            bump,
        })
    }

    pub fn pda_bump_seed(self) -> [u8; 1] {
        [self.bump]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TwapProgramRotation {
    pub market: Pubkey,
    pub old_twap_program: Pubkey,
    pub old_twap_pda: Pubkey,
    pub new_twap_program: Pubkey,
    pub new_twap_pda: Pubkey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TwapProgramConfig {
    pub authority_chain: TwapAuthorityChain,
    pub controller: Pubkey,
    pub market_0_domain: u8,
    pub surplus_buy_burn_bps: u16,
    pub withdraw_accounts: TwapWithdrawAccounts,
}

impl TwapProgramConfig {
    pub fn initialize(
        authority_chain: TwapAuthorityChain,
        controller: Pubkey,
        market_0_domain: u8,
        withdraw_accounts: TwapWithdrawAccounts,
    ) -> Result<Self, SurplusError> {
        Self::initialize_with_buy_burn_bps(
            authority_chain,
            controller,
            market_0_domain,
            MARKET_0_SURPLUS_BUY_BURN_BPS,
            withdraw_accounts,
        )
    }

    pub fn initialize_with_buy_burn_bps(
        authority_chain: TwapAuthorityChain,
        controller: Pubkey,
        market_0_domain: u8,
        surplus_buy_burn_bps: u16,
        withdraw_accounts: TwapWithdrawAccounts,
    ) -> Result<Self, SurplusError> {
        if controller != authority_chain.squads {
            return Err(SurplusError::UnauthorizedController);
        }
        if market_0_domain > 1 {
            return Err(SurplusError::InvalidMarket0Domain);
        }
        validate_surplus_buy_burn_bps(surplus_buy_burn_bps)?;
        withdraw_accounts.validate_for_market(authority_chain.market)?;
        Ok(Self {
            authority_chain,
            controller,
            market_0_domain,
            surplus_buy_burn_bps,
            withdraw_accounts,
        })
    }

    pub fn reconfigure_domain(
        &mut self,
        controller: Pubkey,
        market_0_domain: u8,
    ) -> Result<(), SurplusError> {
        if controller != self.controller {
            return Err(SurplusError::UnauthorizedController);
        }
        if market_0_domain > 1 {
            return Err(SurplusError::InvalidMarket0Domain);
        }
        self.market_0_domain = market_0_domain;
        Ok(())
    }

    pub fn reconfigure_surplus_buy_burn_bps(
        &mut self,
        controller: Pubkey,
        surplus_buy_burn_bps: u16,
    ) -> Result<(), SurplusError> {
        if controller != self.controller {
            return Err(SurplusError::UnauthorizedController);
        }
        validate_surplus_buy_burn_bps(surplus_buy_burn_bps)?;
        self.surplus_buy_burn_bps = surplus_buy_burn_bps;
        Ok(())
    }

    pub fn rotate_twap_program(
        &mut self,
        controller: Pubkey,
        new_twap_program: Pubkey,
        new_withdraw_accounts: TwapWithdrawAccounts,
    ) -> Result<TwapProgramRotation, SurplusError> {
        if controller != self.controller {
            return Err(SurplusError::UnauthorizedController);
        }
        require_nonzero(new_twap_program)?;
        if new_twap_program == self.authority_chain.twap_program {
            return Err(SurplusError::TwapProgramUnchanged);
        }
        new_withdraw_accounts.validate_for_market(self.authority_chain.market)?;

        let old_twap_program = self.authority_chain.twap_program;
        let old_twap_pda = self.authority_chain.twap_pda;
        let new_authority_chain = TwapAuthorityChain::new(
            self.authority_chain.metadao_futarchy,
            self.authority_chain.squads,
            new_twap_program,
            self.authority_chain.percolator_program,
            self.authority_chain.market,
        )?;
        let rotation = TwapProgramRotation {
            market: self.authority_chain.market,
            old_twap_program,
            old_twap_pda,
            new_twap_program,
            new_twap_pda: new_authority_chain.twap_pda,
        };
        self.authority_chain = new_authority_chain;
        self.withdraw_accounts = new_withdraw_accounts;
        Ok(rotation)
    }

    pub fn market_0_default_schedule(
        self,
        start_slot: Slot,
        insurance: Market0Insurance,
    ) -> Result<TwapBuyBurnSchedule, SurplusError> {
        TwapBuyBurnSchedule::market_0_default_with_buy_burn_bps(
            start_slot,
            insurance,
            self.surplus_buy_burn_bps,
        )
    }

    pub fn percolator_withdraw_accounts(
        self,
        accounts: TwapWithdrawAccounts,
    ) -> Result<WithdrawInsuranceAccounts, SurplusError> {
        accounts.validate_for_market(self.authority_chain.market)?;
        if accounts != self.withdraw_accounts {
            return Err(SurplusError::WithdrawAccountsMismatch);
        }
        Ok(WithdrawInsuranceAccounts {
            program: self.withdraw_accounts.percolator_program,
            authority: self.authority_chain.twap_pda,
            market: self.withdraw_accounts.market,
            destination_token: self.withdraw_accounts.twap_pda_collateral_token,
            vault_token: self.withdraw_accounts.market_vault_token,
            vault_authority: self.withdraw_accounts.percolator_vault_authority,
            token_program: self.withdraw_accounts.token_program,
        })
    }

    pub fn retire_current_operator_to_squads_ix(self) -> Instruction {
        update_asset_authority_ix(
            self.withdraw_accounts.percolator_program,
            self.authority_chain.twap_pda,
            self.authority_chain.squads,
            self.authority_chain.market,
            MARKET_0_ASSET_INDEX,
            ASSET_AUTH_INSURANCE_OPERATOR,
        )
    }

    pub fn install_current_operator_from_squads_ix(self) -> Instruction {
        self.replace_current_operator_from_squads_ix()
    }

    pub fn replace_current_operator_from_squads_ix(self) -> Instruction {
        update_market_0_insurance_operator_by_asset_admin_ix(
            self.withdraw_accounts.percolator_program,
            self.authority_chain.squads,
            self.authority_chain.twap_pda,
            self.authority_chain.market,
        )
    }

    pub fn accept_bids_permissionlessly(
        self,
        request: PermissionlessBuyBurnRequest,
        state: &mut TwapBuyBurnState,
        insurance: &mut Market0Insurance,
        bids: &mut [RegisteredBid],
    ) -> Result<PermissionlessBuyBurnExecution, SurplusError> {
        require_nonzero(request.caller)?;
        if state.schedule.surplus_buy_burn_bps != self.surplus_buy_burn_bps
            || insurance.surplus_buy_burn_bps != self.surplus_buy_burn_bps
        {
            return Err(SurplusError::TwapPolicyMismatch);
        }
        let percolator_accounts = self.percolator_withdraw_accounts(request.withdraw_accounts)?;
        let buy_burn = state.execute(insurance, bids, request.slot)?;
        let withdraw_ix = if buy_burn.pulled_from_insurance == 0 {
            None
        } else {
            Some(withdraw_insurance_domain_ix(
                percolator_accounts,
                request.insurance_ledger,
                self.market_0_domain,
                buy_burn.pulled_from_insurance,
            ))
        };
        Ok(PermissionlessBuyBurnExecution {
            caller: request.caller,
            insurance_operator_pda: self.authority_chain.twap_pda,
            domain: self.market_0_domain,
            withdraw_ix,
            buy_burn,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TwapWithdrawAccounts {
    pub percolator_program: Pubkey,
    pub market: Pubkey,
    pub twap_pda_collateral_token: Pubkey,
    pub market_vault_token: Pubkey,
    pub percolator_vault_authority: Pubkey,
    pub token_program: Pubkey,
}

impl TwapWithdrawAccounts {
    pub fn validate_for_market(self, market: Pubkey) -> Result<(), SurplusError> {
        require_nonzero(self.percolator_program)?;
        require_nonzero(self.market)?;
        require_nonzero(self.twap_pda_collateral_token)?;
        require_nonzero(self.market_vault_token)?;
        require_nonzero(self.percolator_vault_authority)?;
        require_nonzero(self.token_program)?;
        if self.market != market {
            return Err(SurplusError::MarketMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionlessBuyBurnRequest {
    pub caller: Pubkey,
    pub slot: Slot,
    pub withdraw_accounts: TwapWithdrawAccounts,
    pub insurance_ledger: Option<Pubkey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Market0Surplus {
    pub recovered_value: Amount,
    pub outstanding_principal: Amount,
}

impl Market0Surplus {
    pub fn surplus(self) -> Amount {
        self.recovered_value
            .saturating_sub(self.outstanding_principal)
    }

    pub fn buy_burn_budget(self) -> Result<Amount, SurplusError> {
        self.buy_burn_budget_at_bps(MARKET_0_SURPLUS_BUY_BURN_BPS)
    }

    pub fn buy_burn_budget_at_bps(self, burn_bps: u16) -> Result<Amount, SurplusError> {
        validate_surplus_buy_burn_bps(burn_bps)?;
        self.surplus()
            .checked_mul(burn_bps as Amount)
            .and_then(|v| v.checked_div(BPS_DENOMINATOR as Amount))
            .ok_or(SurplusError::ArithmeticOverflow)
    }

    pub fn retained_surplus(self) -> Result<Amount, SurplusError> {
        self.retained_surplus_at_bps(MARKET_0_SURPLUS_BUY_BURN_BPS)
    }

    pub fn retained_surplus_at_bps(self, burn_bps: u16) -> Result<Amount, SurplusError> {
        validate_surplus_buy_burn_bps(burn_bps)?;
        self.surplus()
            .checked_sub(self.buy_burn_budget_at_bps(burn_bps)?)
            .ok_or(SurplusError::ArithmeticOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Market0Insurance {
    pub insurance_balance: Amount,
    pub reserved_principal: Amount,
    pub retained_surplus_floor: Amount,
    pub surplus_buy_burn_bps: u16,
}

impl Market0Insurance {
    pub fn new(
        insurance_balance: Amount,
        reserved_principal: Amount,
    ) -> Result<Self, SurplusError> {
        Self::new_with_buy_burn_bps(
            insurance_balance,
            reserved_principal,
            MARKET_0_SURPLUS_BUY_BURN_BPS,
        )
    }

    pub fn new_with_buy_burn_bps(
        insurance_balance: Amount,
        reserved_principal: Amount,
        surplus_buy_burn_bps: u16,
    ) -> Result<Self, SurplusError> {
        validate_surplus_buy_burn_bps(surplus_buy_burn_bps)?;
        Ok(Self {
            insurance_balance,
            reserved_principal,
            retained_surplus_floor: insurance_balance.saturating_sub(reserved_principal),
            surplus_buy_burn_bps,
        })
    }

    pub fn surplus(self) -> Amount {
        self.insurance_balance
            .saturating_sub(self.reserved_principal)
    }

    pub fn expected_floor(self) -> Result<Amount, SurplusError> {
        self.reserved_principal
            .checked_add(self.retained_surplus_floor)
            .ok_or(SurplusError::ArithmeticOverflow)
    }

    pub fn withdrawable_surplus(self) -> Result<Amount, SurplusError> {
        Ok(self
            .insurance_balance
            .saturating_sub(self.expected_floor()?))
    }

    pub fn snapshot(self) -> Market0Surplus {
        Market0Surplus {
            recovered_value: self.insurance_balance,
            outstanding_principal: self.reserved_principal,
        }
    }

    pub fn record_profit(&mut self, amount: Amount) -> Result<(), SurplusError> {
        if amount == 0 {
            return Err(SurplusError::InvalidAmount);
        }
        let floor_before = self.expected_floor()?;
        let floor_deficit = floor_before.saturating_sub(self.insurance_balance);
        let floor_growth_base = amount.saturating_sub(floor_deficit);
        let burn_growth = floor_growth_base
            .checked_mul(self.surplus_buy_burn_bps as Amount)
            .and_then(|v| v.checked_div(BPS_DENOMINATOR as Amount))
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let retained_growth = floor_growth_base
            .checked_sub(burn_growth)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let new_insurance_balance = self
            .insurance_balance
            .checked_add(amount)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let new_retained_surplus_floor = self
            .retained_surplus_floor
            .checked_add(retained_growth)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        self.insurance_balance = new_insurance_balance;
        self.retained_surplus_floor = new_retained_surplus_floor;
        Ok(())
    }

    pub fn set_surplus_buy_burn_bps(
        &mut self,
        surplus_buy_burn_bps: u16,
    ) -> Result<(), SurplusError> {
        validate_surplus_buy_burn_bps(surplus_buy_burn_bps)?;
        self.surplus_buy_burn_bps = surplus_buy_burn_bps;
        Ok(())
    }

    pub fn record_loss(&mut self, amount: Amount) -> Result<(), SurplusError> {
        if amount == 0 {
            return Err(SurplusError::InvalidAmount);
        }
        self.insurance_balance = self
            .insurance_balance
            .checked_sub(amount)
            .ok_or(SurplusError::InsufficientInsuranceSurplus)?;
        Ok(())
    }

    fn pull_surplus(&mut self, amount: Amount) -> Result<(), SurplusError> {
        if amount > self.withdrawable_surplus()? {
            return Err(SurplusError::InsufficientInsuranceSurplus);
        }
        self.insurance_balance = self
            .insurance_balance
            .checked_sub(amount)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisteredBid {
    pub bidder: Pubkey,
    pub coin_atoms: Amount,
    pub usdc_atoms: Amount,
    pub filled_usdc_atoms: Amount,
    pub burned_coin_atoms: Amount,
}

pub type PendingBurnBid = RegisteredBid;

impl RegisteredBid {
    pub fn new(
        bidder: Pubkey,
        coin_atoms: Amount,
        usdc_atoms: Amount,
    ) -> Result<Self, SurplusError> {
        if bidder == Pubkey::default() {
            return Err(SurplusError::InvalidBidder);
        }
        if coin_atoms == 0 || usdc_atoms == 0 {
            return Err(SurplusError::InvalidBid);
        }
        if coin_atoms.checked_mul(usdc_atoms).is_none() {
            return Err(SurplusError::InvalidBid);
        }
        Ok(Self {
            bidder,
            coin_atoms,
            usdc_atoms,
            filled_usdc_atoms: 0,
            burned_coin_atoms: 0,
        })
    }

    pub fn pending_coin_for_usdc(
        bidder: Pubkey,
        coin_atoms: Amount,
        usdc_atoms: Amount,
    ) -> Result<Self, SurplusError> {
        Self::new(bidder, coin_atoms, usdc_atoms)
    }

    pub fn remaining_usdc(self) -> Result<Amount, SurplusError> {
        let remaining_usdc = self
            .usdc_atoms
            .checked_sub(self.filled_usdc_atoms)
            .ok_or(SurplusError::BidOverfilled)?;
        if self.remaining_coin()? == 0 {
            Ok(0)
        } else {
            Ok(remaining_usdc)
        }
    }

    pub fn remaining_coin(self) -> Result<Amount, SurplusError> {
        self.coin_atoms
            .checked_sub(self.burned_coin_atoms)
            .ok_or(SurplusError::BidOverfilled)
    }

    fn coin_for_fill(self, usdc_atoms: Amount) -> Result<Option<Amount>, SurplusError> {
        if usdc_atoms == 0 {
            return Ok(None);
        }
        let remaining_usdc = self.remaining_usdc()?;
        let remaining_coin = self.remaining_coin()?;
        if usdc_atoms > remaining_usdc {
            return Err(SurplusError::BidOverfilled);
        }
        let coin_to_burn = if usdc_atoms == remaining_usdc {
            remaining_coin
        } else {
            let product = remaining_coin
                .checked_mul(usdc_atoms)
                .ok_or(SurplusError::ArithmeticOverflow)?;
            product
                .checked_div(remaining_usdc)
                .ok_or(SurplusError::ArithmeticOverflow)?
        };
        if coin_to_burn == 0 {
            Ok(None)
        } else {
            Ok(Some(coin_to_burn))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BidRefundAtaSnapshot {
    pub address: Pubkey,
    pub account_owner: Pubkey,
    pub token_owner: Pubkey,
    pub mint: Pubkey,
    pub close_authority: Option<Pubkey>,
    pub delegate: Option<Pubkey>,
    pub is_initialized: bool,
    pub is_frozen: bool,
}

impl BidRefundAtaSnapshot {
    pub fn valid_for(
        bidder: Pubkey,
        coin_mint: Pubkey,
        token_program: Pubkey,
        twap_program: Pubkey,
    ) -> Result<Self, SurplusError> {
        require_nonzero(bidder)?;
        require_nonzero(coin_mint)?;
        require_nonzero(token_program)?;
        require_nonzero(twap_program)?;
        let escrow_owner = derive_bid_escrow_pda(bidder, twap_program);
        Ok(Self {
            address: derive_associated_token_address(escrow_owner, coin_mint, token_program),
            account_owner: token_program,
            token_owner: escrow_owner,
            mint: coin_mint,
            close_authority: None,
            delegate: None,
            is_initialized: true,
            is_frozen: false,
        })
    }

    pub fn validate(
        self,
        bidder: Pubkey,
        coin_mint: Pubkey,
        token_program: Pubkey,
        twap_program: Pubkey,
    ) -> Result<(), SurplusError> {
        require_nonzero(self.address)?;
        require_nonzero(self.account_owner)?;
        require_nonzero(self.token_owner)?;
        require_nonzero(self.mint)?;
        let escrow_owner = derive_bid_escrow_pda(bidder, twap_program);
        let close_authority_is_program_controlled = self
            .close_authority
            .map(|authority| authority == escrow_owner)
            .unwrap_or(true);
        if !self.is_initialized
            || self.is_frozen
            || self.address
                != derive_associated_token_address(escrow_owner, coin_mint, token_program)
            || self.account_owner != token_program
            || self.token_owner != escrow_owner
            || self.mint != coin_mint
            || !close_authority_is_program_controlled
            || self.delegate.is_some()
        {
            return Err(SurplusError::InvalidRefundAta);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BidBookEntry {
    pub bid: RegisteredBid,
    pub refund_ata: BidRefundAtaSnapshot,
}

impl RegisteredBid {
    pub fn with_refund_ata(self, refund_ata: BidRefundAtaSnapshot) -> BidBookEntry {
        BidBookEntry {
            bid: self,
            refund_ata,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvictedBid {
    pub bidder: Pubkey,
    pub refund_ata: Option<Pubkey>,
    pub refunded_coin_atoms: Amount,
    pub burned_coin_atoms: Amount,
    pub unfilled_usdc_atoms: Amount,
}

impl EvictedBid {
    fn from_entry(
        entry: BidBookEntry,
        coin_mint: Pubkey,
        token_program: Pubkey,
        twap_program: Pubkey,
        current_refund_ata: BidRefundAtaSnapshot,
    ) -> Result<Self, SurplusError> {
        let bid = entry.bid;
        if bid.bidder == Pubkey::default() {
            return Err(SurplusError::InvalidBidder);
        }
        let remaining_coin = bid.remaining_coin()?;
        if current_refund_ata.address != entry.refund_ata.address {
            return Err(SurplusError::InvalidRefundAta);
        }
        let refund_ata = current_refund_ata
            .validate(bid.bidder, coin_mint, token_program, twap_program)
            .map(|_| current_refund_ata.address)
            .ok();
        Ok(Self {
            bidder: bid.bidder,
            refund_ata,
            refunded_coin_atoms: if refund_ata.is_some() {
                remaining_coin
            } else {
                0
            },
            burned_coin_atoms: if refund_ata.is_some() {
                0
            } else {
                remaining_coin
            },
            unfilled_usdc_atoms: bid
                .usdc_atoms
                .checked_sub(bid.filled_usdc_atoms)
                .ok_or(SurplusError::BidOverfilled)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BidPlacement {
    pub index: usize,
    pub evicted: Option<EvictedBid>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WithdrawnBid {
    pub bidder: Pubkey,
    pub escrow_ata: Pubkey,
    pub withdrawn_coin_atoms: Amount,
    pub unfilled_usdc_atoms: Amount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BidBook {
    pub coin_mint: Pubkey,
    pub token_program: Pubkey,
    pub twap_program: Pubkey,
    pub bids: [Option<BidBookEntry>; MAX_TWAP_BIDS_PER_EXECUTION],
}

impl BidBook {
    pub fn new(
        coin_mint: Pubkey,
        token_program: Pubkey,
        twap_program: Pubkey,
    ) -> Result<Self, SurplusError> {
        require_nonzero(coin_mint)?;
        require_nonzero(token_program)?;
        require_nonzero(twap_program)?;
        Ok(Self {
            coin_mint,
            token_program,
            twap_program,
            bids: [None; MAX_TWAP_BIDS_PER_EXECUTION],
        })
    }

    pub fn place_bid(
        &mut self,
        bid: RegisteredBid,
        caller: Pubkey,
        refund_ata: BidRefundAtaSnapshot,
        evicted_refund_ata: Option<BidRefundAtaSnapshot>,
    ) -> Result<BidPlacement, SurplusError> {
        validate_fresh_bid(bid)?;
        require_nonzero(caller)?;
        if caller != bid.bidder {
            return Err(SurplusError::UnauthorizedBidder);
        }
        if self.active_bid_index_for(bid.bidder)?.is_some() {
            return Err(SurplusError::ActiveBidExists);
        }
        refund_ata.validate(
            bid.bidder,
            self.coin_mint,
            self.token_program,
            self.twap_program,
        )?;
        let entry = bid.with_refund_ata(refund_ata);
        if let Some(placement) = self.reusable_slot(evicted_refund_ata)? {
            self.bids[placement.index] = Some(entry);
            return Ok(placement);
        }

        let Some((weakest_index, weakest_entry)) = self.weakest_active_bid()? else {
            return Err(SurplusError::InvalidBid);
        };
        let incoming = rankable_bid(weakest_index, bid).ok_or(SurplusError::InvalidBid)?;
        let weakest =
            rankable_bid(weakest_index, weakest_entry.bid).ok_or(SurplusError::InvalidBid)?;
        if compare_bid_quality(incoming, weakest)? != Ordering::Greater {
            return Err(SurplusError::BidNotCompetitive);
        }

        let placement = BidPlacement {
            index: weakest_index,
            evicted: Some(EvictedBid::from_entry(
                weakest_entry,
                self.coin_mint,
                self.token_program,
                self.twap_program,
                evicted_refund_ata.ok_or(SurplusError::InvalidRefundAta)?,
            )?),
        };
        self.bids[weakest_index] = Some(entry);
        Ok(placement)
    }

    pub fn withdraw_bid(
        &mut self,
        bidder: Pubkey,
        caller: Pubkey,
        current_escrow_ata: BidRefundAtaSnapshot,
    ) -> Result<WithdrawnBid, SurplusError> {
        require_nonzero(bidder)?;
        require_nonzero(caller)?;
        if caller != bidder {
            return Err(SurplusError::UnauthorizedBidder);
        }
        let Some(index) = self.active_bid_index_for(bidder)? else {
            return Err(SurplusError::BidNotFound);
        };
        let entry = self.bids[index].ok_or(SurplusError::BidNotFound)?;
        current_escrow_ata.validate(
            bidder,
            self.coin_mint,
            self.token_program,
            self.twap_program,
        )?;
        if current_escrow_ata.address != entry.refund_ata.address {
            return Err(SurplusError::InvalidRefundAta);
        }

        let withdrawn = WithdrawnBid {
            bidder,
            escrow_ata: current_escrow_ata.address,
            withdrawn_coin_atoms: entry.bid.remaining_coin()?,
            unfilled_usdc_atoms: entry
                .bid
                .usdc_atoms
                .checked_sub(entry.bid.filled_usdc_atoms)
                .ok_or(SurplusError::BidOverfilled)?,
        };
        self.bids[index] = None;
        Ok(withdrawn)
    }

    pub fn close_bid_escrow(&self, bidder: Pubkey, caller: Pubkey) -> Result<Pubkey, SurplusError> {
        require_nonzero(bidder)?;
        require_nonzero(caller)?;
        if caller != bidder {
            return Err(SurplusError::UnauthorizedBidder);
        }
        if self.active_bid_index_for(bidder)?.is_some() {
            return Err(SurplusError::ActiveBidExists);
        }
        let escrow_owner = derive_bid_escrow_pda(bidder, self.twap_program);
        Ok(derive_associated_token_address(
            escrow_owner,
            self.coin_mint,
            self.token_program,
        ))
    }

    fn reusable_slot(
        &self,
        evicted_refund_ata: Option<BidRefundAtaSnapshot>,
    ) -> Result<Option<BidPlacement>, SurplusError> {
        for (index, bid) in self.bids.iter().enumerate() {
            let Some(entry) = bid else {
                return Ok(Some(BidPlacement {
                    index,
                    evicted: None,
                }));
            };
            if entry.bid.remaining_coin()? == 0 || entry.bid.remaining_usdc()? == 0 {
                let evicted = if entry.bid.remaining_coin()? == 0 {
                    None
                } else {
                    Some(EvictedBid::from_entry(
                        *entry,
                        self.coin_mint,
                        self.token_program,
                        self.twap_program,
                        evicted_refund_ata.ok_or(SurplusError::InvalidRefundAta)?,
                    )?)
                };
                return Ok(Some(BidPlacement { index, evicted }));
            }
        }
        Ok(None)
    }

    fn active_bid_index_for(&self, bidder: Pubkey) -> Result<Option<usize>, SurplusError> {
        for (index, entry) in self.bids.iter().enumerate() {
            let Some(entry) = entry else {
                continue;
            };
            if entry.bid.bidder == bidder && entry.bid.remaining_coin()? != 0 {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    fn weakest_active_bid(&self) -> Result<Option<(usize, BidBookEntry)>, SurplusError> {
        let mut weakest: Option<RankableBid> = None;
        for (index, bid) in self.bids.iter().enumerate() {
            let Some(entry) = bid else {
                continue;
            };
            let Some(candidate) = rankable_bid(index, entry.bid) else {
                continue;
            };
            let should_replace = match weakest {
                Some(current) => {
                    let quality = compare_bid_quality(candidate, current)?;
                    quality == Ordering::Less
                        || (quality == Ordering::Equal && candidate.index < current.index)
                }
                None => true,
            };
            if should_replace {
                weakest = Some(candidate);
            }
        }
        Ok(weakest.map(|bid| (bid.index, self.bids[bid.index].unwrap())))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TwapBuyBurnSchedule {
    pub start_slot: Slot,
    pub end_slot: Slot,
    pub total_budget: Amount,
    pub interval_slots: Slot,
    pub interval_count: Slot,
    pub surplus_buy_burn_bps: u16,
}

impl TwapBuyBurnSchedule {
    pub fn new(
        start_slot: Slot,
        end_slot: Slot,
        insurance: Market0Insurance,
    ) -> Result<Self, SurplusError> {
        Self::new_with_buy_burn_bps(
            start_slot,
            end_slot,
            insurance,
            MARKET_0_SURPLUS_BUY_BURN_BPS,
        )
    }

    pub fn new_with_buy_burn_bps(
        start_slot: Slot,
        end_slot: Slot,
        insurance: Market0Insurance,
        surplus_buy_burn_bps: u16,
    ) -> Result<Self, SurplusError> {
        validate_surplus_buy_burn_bps(surplus_buy_burn_bps)?;
        if insurance.surplus_buy_burn_bps != surplus_buy_burn_bps {
            return Err(SurplusError::TwapPolicyMismatch);
        }
        if end_slot <= start_slot {
            return Err(SurplusError::InvalidTwapWindow);
        }
        let interval_count = end_slot
            .checked_sub(start_slot)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        validate_twap_interval_count(interval_count)?;
        Ok(Self {
            start_slot,
            end_slot,
            total_budget: insurance.withdrawable_surplus()?,
            interval_slots: 1,
            interval_count,
            surplus_buy_burn_bps,
        })
    }

    pub fn market_0_default(
        start_slot: Slot,
        insurance: Market0Insurance,
    ) -> Result<Self, SurplusError> {
        Self::market_0_default_with_buy_burn_bps(
            start_slot,
            insurance,
            MARKET_0_SURPLUS_BUY_BURN_BPS,
        )
    }

    pub fn market_0_default_with_buy_burn_bps(
        start_slot: Slot,
        insurance: Market0Insurance,
        surplus_buy_burn_bps: u16,
    ) -> Result<Self, SurplusError> {
        Self::new_interval_with_buy_burn_bps(
            start_slot,
            TWAP_INTERVAL_SLOTS,
            TWAP_INTERVAL_COUNT,
            insurance,
            surplus_buy_burn_bps,
        )
    }

    pub fn new_interval(
        start_slot: Slot,
        interval_slots: Slot,
        interval_count: Slot,
        insurance: Market0Insurance,
    ) -> Result<Self, SurplusError> {
        Self::new_interval_with_buy_burn_bps(
            start_slot,
            interval_slots,
            interval_count,
            insurance,
            MARKET_0_SURPLUS_BUY_BURN_BPS,
        )
    }

    pub fn new_interval_with_buy_burn_bps(
        start_slot: Slot,
        interval_slots: Slot,
        interval_count: Slot,
        insurance: Market0Insurance,
        surplus_buy_burn_bps: u16,
    ) -> Result<Self, SurplusError> {
        validate_surplus_buy_burn_bps(surplus_buy_burn_bps)?;
        if insurance.surplus_buy_burn_bps != surplus_buy_burn_bps {
            return Err(SurplusError::TwapPolicyMismatch);
        }
        if interval_slots == 0 || interval_count == 0 {
            return Err(SurplusError::InvalidTwapWindow);
        }
        validate_twap_interval_count(interval_count)?;
        let duration = interval_slots
            .checked_mul(interval_count)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let end_slot = start_slot
            .checked_add(duration)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        Ok(Self {
            start_slot,
            end_slot,
            total_budget: insurance.withdrawable_surplus()?,
            interval_slots,
            interval_count,
            surplus_buy_burn_bps,
        })
    }

    pub fn budget_released_by(self, slot: Slot) -> Result<Amount, SurplusError> {
        if slot <= self.start_slot {
            return Ok(0);
        }
        if slot >= self.end_slot {
            return Ok(self.total_budget);
        }
        let elapsed_slots = slot
            .checked_sub(self.start_slot)
            .ok_or(SurplusError::ArithmeticOverflow)? as Amount;
        let intervals_elapsed =
            (elapsed_slots / self.interval_slots as Amount).min(self.interval_count as Amount);
        self.total_budget
            .checked_mul(intervals_elapsed)
            .and_then(|v| v.checked_div(self.interval_count as Amount))
            .ok_or(SurplusError::ArithmeticOverflow)
    }

    fn next_interval_target_after(self, pulled: Amount) -> Result<Amount, SurplusError> {
        if pulled >= self.total_budget {
            return Ok(self.total_budget);
        }
        if self.total_budget == 0 {
            return Ok(0);
        }
        let mut interval = pulled
            .checked_mul(self.interval_count as Amount)
            .and_then(|v| v.checked_div(self.total_budget))
            .ok_or(SurplusError::ArithmeticOverflow)?
            .checked_add(1)
            .ok_or(SurplusError::ArithmeticOverflow)? as Slot;
        while interval <= self.interval_count {
            let target = self
                .total_budget
                .checked_mul(interval as Amount)
                .and_then(|v| v.checked_div(self.interval_count as Amount))
                .ok_or(SurplusError::ArithmeticOverflow)?;
            if target > pulled {
                return Ok(target);
            }
            interval = interval
                .checked_add(1)
                .ok_or(SurplusError::ArithmeticOverflow)?;
        }
        Ok(self.total_budget)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TwapBuyBurnState {
    pub schedule: TwapBuyBurnSchedule,
    pub pulled_from_insurance: Amount,
    pub burned_coin_atoms: Amount,
}

impl TwapBuyBurnState {
    pub fn new(schedule: TwapBuyBurnSchedule) -> Self {
        Self {
            schedule,
            pulled_from_insurance: 0,
            burned_coin_atoms: 0,
        }
    }

    pub fn executable_budget(
        &self,
        insurance: Market0Insurance,
        bids: &[RegisteredBid],
        slot: Slot,
    ) -> Result<Amount, SurplusError> {
        Ok(self.plan_execution(insurance, bids, slot)?.pull_amount)
    }

    pub fn next_releasable_budget_by(self, slot: Slot) -> Result<Amount, SurplusError> {
        let released = self.schedule.budget_released_by(slot)?;
        if released <= self.pulled_from_insurance {
            return Ok(0);
        }
        let next_target = self
            .schedule
            .next_interval_target_after(self.pulled_from_insurance)?;
        let capped_target = min(released, next_target);
        capped_target
            .checked_sub(self.pulled_from_insurance)
            .ok_or(SurplusError::ArithmeticOverflow)
    }

    pub fn execute(
        &mut self,
        insurance: &mut Market0Insurance,
        bids: &mut [RegisteredBid],
        slot: Slot,
    ) -> Result<BuyBurnExecution, SurplusError> {
        let plan = self.plan_execution(*insurance, bids, slot)?;
        if plan.pull_amount == 0 {
            return Ok(BuyBurnExecution::default());
        }

        let new_pulled_from_insurance = self
            .pulled_from_insurance
            .checked_add(plan.pull_amount)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let new_burned_coin_atoms = self
            .burned_coin_atoms
            .checked_add(plan.burned_coin_atoms)
            .ok_or(SurplusError::ArithmeticOverflow)?;

        insurance.pull_surplus(plan.pull_amount)?;
        for update in plan.bid_updates {
            bids[update.index].filled_usdc_atoms = update.filled_usdc_atoms;
            bids[update.index].burned_coin_atoms = update.burned_coin_atoms;
        }
        self.pulled_from_insurance = new_pulled_from_insurance;
        self.burned_coin_atoms = new_burned_coin_atoms;

        Ok(BuyBurnExecution {
            pulled_from_insurance: plan.pull_amount,
            burned_coin_atoms: plan.burned_coin_atoms,
            fills: plan.fills,
        })
    }

    fn plan_execution(
        &self,
        insurance: Market0Insurance,
        bids: &[RegisteredBid],
        slot: Slot,
    ) -> Result<PlannedBuyBurn, SurplusError> {
        let budget_cap = min(
            self.next_releasable_budget_by(slot)?,
            insurance.withdrawable_surplus()?,
        );
        if budget_cap == 0 {
            return Ok(PlannedBuyBurn::default());
        }

        let top_bid_indices = top_bid_indices(bids)?;
        let mut remaining = budget_cap;
        let mut burned_coin_atoms = 0u128;
        let mut fills = Vec::new();
        let mut bid_updates = Vec::new();
        for index in top_bid_indices {
            if remaining == 0 {
                break;
            }
            let bid = bids[index];
            let fill_usdc_atoms = min(remaining, bid.remaining_usdc()?);
            let Some(coin_atoms) = bid.coin_for_fill(fill_usdc_atoms)? else {
                continue;
            };
            let new_filled_usdc_atoms = bid
                .filled_usdc_atoms
                .checked_add(fill_usdc_atoms)
                .ok_or(SurplusError::ArithmeticOverflow)?;
            let new_burned_coin_atoms = bid
                .burned_coin_atoms
                .checked_add(coin_atoms)
                .ok_or(SurplusError::ArithmeticOverflow)?;
            remaining = remaining
                .checked_sub(fill_usdc_atoms)
                .ok_or(SurplusError::ArithmeticOverflow)?;
            burned_coin_atoms = burned_coin_atoms
                .checked_add(coin_atoms)
                .ok_or(SurplusError::ArithmeticOverflow)?;
            let fill = BuyBurnFill {
                bidder: bid.bidder,
                usdc_atoms: fill_usdc_atoms,
                coin_atoms,
            };
            bid_updates.push(PlannedBidUpdate {
                index,
                filled_usdc_atoms: new_filled_usdc_atoms,
                burned_coin_atoms: new_burned_coin_atoms,
            });
            fills.push(fill);
        }
        Ok(PlannedBuyBurn {
            pull_amount: budget_cap
                .checked_sub(remaining)
                .ok_or(SurplusError::ArithmeticOverflow)?,
            burned_coin_atoms,
            bid_updates,
            fills,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuyBurnExecution {
    pub pulled_from_insurance: Amount,
    pub burned_coin_atoms: Amount,
    pub fills: Vec<BuyBurnFill>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionlessBuyBurnExecution {
    pub caller: Pubkey,
    pub insurance_operator_pda: Pubkey,
    pub domain: u8,
    pub withdraw_ix: Option<Instruction>,
    pub buy_burn: BuyBurnExecution,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuyBurnFill {
    pub bidder: Pubkey,
    pub usdc_atoms: Amount,
    pub coin_atoms: Amount,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PlannedBuyBurn {
    pull_amount: Amount,
    burned_coin_atoms: Amount,
    bid_updates: Vec<PlannedBidUpdate>,
    fills: Vec<BuyBurnFill>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlannedBidUpdate {
    index: usize,
    filled_usdc_atoms: Amount,
    burned_coin_atoms: Amount,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SurplusError {
    #[error("arithmetic overflow")]
    ArithmeticOverflow,
    #[error("invariant violation")]
    InvariantViolation,
    #[error("invalid TWAP window")]
    InvalidTwapWindow,
    #[error("invalid amount")]
    InvalidAmount,
    #[error("insufficient market-0 insurance surplus")]
    InsufficientInsuranceSurplus,
    #[error("invalid bidder")]
    InvalidBidder,
    #[error("invalid bid")]
    InvalidBid,
    #[error("invalid refund ATA")]
    InvalidRefundAta,
    #[error("bid is overfilled")]
    BidOverfilled,
    #[error("bid is not competitive")]
    BidNotCompetitive,
    #[error("active bid exists")]
    ActiveBidExists,
    #[error("bid not found")]
    BidNotFound,
    #[error("bidder authorization is required")]
    UnauthorizedBidder,
    #[error("too many bids for one TWAP execution")]
    TooManyBids,
    #[error("unauthorized TWAP controller")]
    UnauthorizedController,
    #[error("market does not match TWAP authority chain")]
    MarketMismatch,
    #[error("withdraw accounts do not match configured TWAP accounts")]
    WithdrawAccountsMismatch,
    #[error("invalid market-0 domain")]
    InvalidMarket0Domain,
    #[error("invalid pubkey")]
    InvalidPubkey,
    #[error("TWAP state policy does not match configured policy")]
    TwapPolicyMismatch,
    #[error("new TWAP program is unchanged")]
    TwapProgramUnchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RankableBid {
    index: usize,
    remaining_coin: Amount,
    remaining_usdc: Amount,
}

fn top_bid_indices(bids: &[RegisteredBid]) -> Result<Vec<usize>, SurplusError> {
    if bids.len() > MAX_TWAP_BIDS_PER_EXECUTION {
        return Err(SurplusError::TooManyBids);
    }
    let mut top = Vec::with_capacity(min(bids.len(), MAX_TWAP_BIDS_PER_EXECUTION));
    for (index, bid) in bids.iter().enumerate() {
        let Some(candidate) = rankable_bid(index, *bid) else {
            continue;
        };

        let mut insert_at = top.len();
        for (rank, selected) in top.iter().enumerate() {
            if compare_bid_quality(candidate, *selected)? == Ordering::Greater {
                insert_at = rank;
                break;
            }
        }

        if insert_at < top.len() {
            top.insert(insert_at, candidate);
            if top.len() > MAX_TWAP_BIDS_PER_EXECUTION {
                top.pop();
            }
        } else if top.len() < MAX_TWAP_BIDS_PER_EXECUTION {
            top.push(candidate);
        }
    }
    Ok(top.into_iter().map(|bid| bid.index).collect())
}

fn rankable_bid(index: usize, bid: RegisteredBid) -> Option<RankableBid> {
    if bid.bidder == Pubkey::default() || bid.coin_atoms == 0 || bid.usdc_atoms == 0 {
        return None;
    }
    let remaining_coin = bid.coin_atoms.checked_sub(bid.burned_coin_atoms)?;
    let remaining_usdc = bid.usdc_atoms.checked_sub(bid.filled_usdc_atoms)?;
    if remaining_coin == 0 || remaining_usdc == 0 {
        return None;
    }
    remaining_coin.checked_mul(remaining_usdc)?;
    Some(RankableBid {
        index,
        remaining_coin,
        remaining_usdc,
    })
}

fn compare_bid_quality(a: RankableBid, b: RankableBid) -> Result<Ordering, SurplusError> {
    compare_fraction(
        a.remaining_coin,
        a.remaining_usdc,
        b.remaining_coin,
        b.remaining_usdc,
    )
}

fn validate_fresh_bid(bid: RegisteredBid) -> Result<(), SurplusError> {
    if bid.bidder == Pubkey::default() {
        return Err(SurplusError::InvalidBidder);
    }
    if bid.coin_atoms == 0
        || bid.usdc_atoms == 0
        || bid.filled_usdc_atoms != 0
        || bid.burned_coin_atoms != 0
        || bid.coin_atoms.checked_mul(bid.usdc_atoms).is_none()
    {
        return Err(SurplusError::InvalidBid);
    }
    Ok(())
}

fn compare_fraction(
    mut left_num: Amount,
    mut left_den: Amount,
    mut right_num: Amount,
    mut right_den: Amount,
) -> Result<Ordering, SurplusError> {
    if left_den == 0 || right_den == 0 {
        return Err(SurplusError::InvalidBid);
    }
    let mut reversed = false;
    loop {
        let left_quotient = left_num
            .checked_div(left_den)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let right_quotient = right_num
            .checked_div(right_den)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let quotient_ordering = left_quotient.cmp(&right_quotient);
        if quotient_ordering != Ordering::Equal {
            return Ok(apply_reversal(quotient_ordering, reversed));
        }

        let left_remainder = left_num
            .checked_rem(left_den)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        let right_remainder = right_num
            .checked_rem(right_den)
            .ok_or(SurplusError::ArithmeticOverflow)?;
        match (left_remainder == 0, right_remainder == 0) {
            (true, true) => return Ok(Ordering::Equal),
            (true, false) => return Ok(apply_reversal(Ordering::Less, reversed)),
            (false, true) => return Ok(apply_reversal(Ordering::Greater, reversed)),
            (false, false) => {
                left_num = left_den;
                left_den = left_remainder;
                right_num = right_den;
                right_den = right_remainder;
                reversed = !reversed;
            }
        }
    }
}

fn apply_reversal(ordering: Ordering, reversed: bool) -> Ordering {
    if reversed {
        ordering.reverse()
    } else {
        ordering
    }
}

fn validate_twap_interval_count(interval_count: Slot) -> Result<(), SurplusError> {
    if interval_count == 0 || interval_count > MAX_TWAP_INTERVAL_COUNT {
        Err(SurplusError::InvalidTwapWindow)
    } else {
        Ok(())
    }
}

fn validate_surplus_buy_burn_bps(surplus_buy_burn_bps: u16) -> Result<(), SurplusError> {
    if surplus_buy_burn_bps > BPS_DENOMINATOR {
        Err(SurplusError::InvalidAmount)
    } else {
        Ok(())
    }
}

fn require_nonzero(pubkey: Pubkey) -> Result<(), SurplusError> {
    if pubkey == Pubkey::default() {
        Err(SurplusError::InvalidPubkey)
    } else {
        Ok(())
    }
}
