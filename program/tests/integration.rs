//! Integration tests for the insurance deposit program.
//!
//! Uses LiteSVM with BPF binaries for both percolator-prog and rewards-program.
//!
//! Build both programs first:
//!   cd ../percolator-prog && cargo build-sbf
//!   cargo build-sbf --manifest-path governance/Cargo.toml
//!   cargo build-sbf --manifest-path program/Cargo.toml
//!
//! Run: cargo test --test integration

use governance_adapter::{authority_address as governance_authority_address, id as governance_program_id};
use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
    transaction::Transaction,
};
use spl_token::state::{Account as TokenAccount, AccountState, Mint};
use std::path::PathBuf;

// Production SLAB_LEN for SBF target.
// Must match the BPF binary.
const SLAB_LEN: usize = 1525688;
const MAX_ACCOUNTS: usize = 4096;

const PYTH_RECEIVER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x0c, 0xb7, 0xfa, 0xbb, 0x52, 0xf7, 0xa6, 0x48, 0xbb, 0x5b, 0x31, 0x7d, 0x9a, 0x01, 0x8b,
    0x90, 0x57, 0xcb, 0x02, 0x47, 0x74, 0xfa, 0xfe, 0x01, 0xe6, 0xc4, 0xdf, 0x98, 0xcc, 0x38,
    0x58, 0x81,
]);

// All-zeros feed_id = Hyperp mode (no external oracle read at init)
const TEST_FEED_ID: [u8; 32] = [0u8; 32];

fn percolator_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // go up from program/
    path.push("../percolator-prog/target/deploy/percolator_prog.so");
    let path = path.canonicalize().unwrap_or(path);
    assert!(
        path.exists(),
        "Percolator BPF not found at {:?}. Run: cd ../percolator-prog && cargo build-sbf",
        path
    );
    path
}

fn rewards_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // go up from program/
    path.push("target/deploy/rewards_program.so");
    assert!(
        path.exists(),
        "Rewards BPF not found at {:?}. Run: cargo build-sbf",
        path
    );
    path
}

fn governance_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // go up from program/
    path.push("target/deploy/governance_adapter.so");
    let path = path.canonicalize().unwrap_or(path);
    assert!(
        path.exists(),
        "Governance adapter BPF not found at {:?}. Run: cargo build-sbf --manifest-path governance/Cargo.toml",
        path
    );
    path
}

fn make_token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; TokenAccount::LEN];
    let mut account = TokenAccount::default();
    account.mint = *mint;
    account.owner = *owner;
    account.amount = amount;
    account.state = AccountState::Initialized;
    TokenAccount::pack(account, &mut data).unwrap();
    data
}

fn make_mint_data_with_authority(mint_authority: &Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; Mint::LEN];
    let mint = Mint {
        mint_authority: solana_sdk::program_option::COption::Some(*mint_authority),
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: solana_sdk::program_option::COption::None,
    };
    Mint::pack(mint, &mut data).unwrap();
    data
}

fn make_mint_data_no_authority() -> Vec<u8> {
    let mut data = vec![0u8; Mint::LEN];
    let mint = Mint {
        mint_authority: solana_sdk::program_option::COption::None,
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: solana_sdk::program_option::COption::None,
    };
    Mint::pack(mint, &mut data).unwrap();
    data
}

fn make_mint_data_with_freeze(mint_authority: &Pubkey, freeze_authority: &Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; Mint::LEN];
    let mint = Mint {
        mint_authority: solana_sdk::program_option::COption::Some(*mint_authority),
        supply: 0,
        decimals: 6,
        is_initialized: true,
        freeze_authority: solana_sdk::program_option::COption::Some(*freeze_authority),
    };
    Mint::pack(mint, &mut data).unwrap();
    data
}

fn make_pyth_data(
    feed_id: &[u8; 32],
    price: i64,
    expo: i32,
    conf: u64,
    publish_time: i64,
) -> Vec<u8> {
    let mut data = vec![0u8; 134];
    data[42..74].copy_from_slice(feed_id);
    data[74..82].copy_from_slice(&price.to_le_bytes());
    data[82..90].copy_from_slice(&conf.to_le_bytes());
    data[90..94].copy_from_slice(&expo.to_le_bytes());
    data[94..102].copy_from_slice(&publish_time.to_le_bytes());
    data
}

// ============================================================================
// Percolator instruction encoders
// ============================================================================

fn encode_init_market(
    admin: &Pubkey,
    mint: &Pubkey,
    feed_id: &[u8; 32],
    trading_fee_bps: u64,
) -> Vec<u8> {
    let mut data = vec![0u8];
    data.extend_from_slice(admin.as_ref());
    data.extend_from_slice(mint.as_ref());
    data.extend_from_slice(feed_id);
    data.extend_from_slice(&86400u64.to_le_bytes()); // max_staleness_secs
    data.extend_from_slice(&500u16.to_le_bytes()); // conf_filter_bps
    data.push(0u8); // invert
    data.extend_from_slice(&0u32.to_le_bytes()); // unit_scale
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // initial_mark_price_e6 (Hyperp needs > 0)
    // Per-market admin limits
    data.extend_from_slice(&0u128.to_le_bytes()); // maintenance_fee_per_slot (0 = disabled)
    data.extend_from_slice(&10_000_000_000_000_000u128.to_le_bytes()); // max_insurance_floor
    data.extend_from_slice(&0u64.to_le_bytes()); // min_oracle_price_cap_e2bps
    // RiskParams (v12.17 format)
    data.extend_from_slice(&0u64.to_le_bytes()); // h_min
    data.extend_from_slice(&500u64.to_le_bytes()); // maintenance_margin_bps
    data.extend_from_slice(&1000u64.to_le_bytes()); // initial_margin_bps
    data.extend_from_slice(&trading_fee_bps.to_le_bytes());
    data.extend_from_slice(&(MAX_ACCOUNTS as u64).to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes()); // new_account_fee
    data.extend_from_slice(&0u128.to_le_bytes()); // insurance_floor
    data.extend_from_slice(&1u64.to_le_bytes()); // h_max (must be > 0, >= h_min)
    data.extend_from_slice(&1000u64.to_le_bytes()); // max_crank_staleness_slots
    data.extend_from_slice(&50u64.to_le_bytes()); // liquidation_fee_bps
    data.extend_from_slice(&1_000_000_000_000u128.to_le_bytes()); // liquidation_fee_cap
    data.extend_from_slice(&100u64.to_le_bytes()); // resolve_price_deviation_bps
    data.extend_from_slice(&0u128.to_le_bytes()); // min_liquidation_abs
    data.extend_from_slice(&100u128.to_le_bytes()); // min_initial_deposit
    data.extend_from_slice(&1u128.to_le_bytes()); // min_nonzero_mm_req
    data.extend_from_slice(&2u128.to_le_bytes()); // min_nonzero_im_req
    // Extended tail (66 bytes) — enable insurance withdrawal + permissionless resolve + force close
    data.extend_from_slice(&10_000u16.to_le_bytes()); // insurance_withdraw_max_bps = 100% (full drain per call)
    data.extend_from_slice(&1u64.to_le_bytes()); // insurance_withdraw_cooldown_slots = 1 slot
    data.extend_from_slice(&2000u64.to_le_bytes()); // permissionless_resolve_stale_slots (> max_crank)
    data.extend_from_slice(&500u64.to_le_bytes()); // funding_horizon_slots
    data.extend_from_slice(&100u64.to_le_bytes()); // funding_k_bps
    data.extend_from_slice(&500i64.to_le_bytes()); // funding_max_premium_bps
    data.extend_from_slice(&1_000i64.to_le_bytes()); // funding_max_e9_per_slot
    data.extend_from_slice(&0u64.to_le_bytes()); // mark_min_fee
    data.extend_from_slice(&100u64.to_le_bytes()); // force_close_delay_slots
    data
}

fn encode_init_lp(matcher: &Pubkey, ctx: &Pubkey, fee: u64) -> Vec<u8> {
    let mut data = vec![2u8];
    data.extend_from_slice(matcher.as_ref());
    data.extend_from_slice(ctx.as_ref());
    data.extend_from_slice(&fee.to_le_bytes());
    data
}

fn encode_init_user(fee: u64) -> Vec<u8> {
    let mut data = vec![1u8];
    data.extend_from_slice(&fee.to_le_bytes());
    data
}

fn encode_deposit(user_idx: u16, amount: u64) -> Vec<u8> {
    let mut data = vec![3u8];
    data.extend_from_slice(&user_idx.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn encode_trade(lp: u16, user: u16, size: i128) -> Vec<u8> {
    let mut data = vec![6u8];
    data.extend_from_slice(&lp.to_le_bytes());
    data.extend_from_slice(&user.to_le_bytes());
    data.extend_from_slice(&size.to_le_bytes());
    data
}

fn encode_update_admin(new_admin: &Pubkey) -> Vec<u8> {
    // Tag 32 = UpdateAuthority, kind 0 = AUTHORITY_ADMIN (was tag 12 UpdateAdmin)
    let mut data = vec![32u8, 0u8];
    data.extend_from_slice(new_admin.as_ref());
    data
}

fn encode_set_risk_threshold(new_threshold: u128) -> Vec<u8> {
    let mut data = vec![11u8];
    data.extend_from_slice(&new_threshold.to_le_bytes());
    data
}

fn encode_close_slab() -> Vec<u8> {
    vec![13u8]
}

fn encode_update_config() -> Vec<u8> {
    let mut data = vec![14u8];
    data.extend_from_slice(&100u64.to_le_bytes());
    data.extend_from_slice(&10u64.to_le_bytes());
    data.extend_from_slice(&1_000_000u128.to_le_bytes());
    data.extend_from_slice(&100i64.to_le_bytes());
    data.extend_from_slice(&10i64.to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes());
    data.extend_from_slice(&50u64.to_le_bytes());
    data.extend_from_slice(&10u64.to_le_bytes());
    data.extend_from_slice(&1000u64.to_le_bytes());
    data.extend_from_slice(&1000u64.to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes());
    data.extend_from_slice(&u128::MAX.to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes());
    data
}

fn encode_set_maintenance_fee(new_fee: u128) -> Vec<u8> {
    let mut data = vec![15u8];
    data.extend_from_slice(&new_fee.to_le_bytes());
    data
}

fn encode_set_oracle_authority(new_authority: &Pubkey) -> Vec<u8> {
    let mut data = vec![16u8];
    data.extend_from_slice(new_authority.as_ref());
    data
}

fn encode_set_oracle_price_cap(max_change_e2bps: u64) -> Vec<u8> {
    let mut data = vec![18u8];
    data.extend_from_slice(&max_change_e2bps.to_le_bytes());
    data
}

fn encode_resolve_market() -> Vec<u8> {
    vec![19u8]
}

fn encode_withdraw_insurance() -> Vec<u8> {
    vec![20u8]
}

fn encode_admin_force_close(user_idx: u16) -> Vec<u8> {
    let mut data = vec![21u8];
    data.extend_from_slice(&user_idx.to_le_bytes());
    data
}

fn encode_set_insurance_withdraw_policy(
    authority: &Pubkey,
    min_withdraw_base: u128,
    max_withdraw_bps: u64,
    cooldown_slots: u64,
) -> Vec<u8> {
    let mut data = vec![22u8];
    data.extend_from_slice(authority.as_ref());
    data.extend_from_slice(&min_withdraw_base.to_le_bytes());
    data.extend_from_slice(&max_withdraw_bps.to_le_bytes());
    data.extend_from_slice(&cooldown_slots.to_le_bytes());
    data
}

fn encode_topup_insurance(amount: u64) -> Vec<u8> {
    let mut data = vec![9u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

// ============================================================================
// Rewards instruction encoders
// ============================================================================

fn encode_init_coin_config() -> Vec<u8> {
    vec![3u8] // tag = IX_INIT_COIN_CONFIG
}

fn encode_init_market_rewards(n: u64, epoch_slots: u64) -> Vec<u8> {
    let mut data = vec![0u8]; // tag = IX_INIT_MARKET_REWARDS
    data.extend_from_slice(&n.to_le_bytes());
    data.extend_from_slice(&epoch_slots.to_le_bytes());
    data
}

fn encode_stake(amount: u64) -> Vec<u8> {
    let mut data = vec![1u8]; // tag = IX_STAKE
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn encode_unstake(amount: u64) -> Vec<u8> {
    let mut data = vec![2u8]; // tag = IX_UNSTAKE
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn encode_claim_stake_rewards() -> Vec<u8> {
    vec![4u8] // tag = IX_CLAIM_STAKE_REWARDS
}

fn encode_governance_init_authority() -> Vec<u8> {
    vec![0u8]
}

fn encode_governance_init_coin_config() -> Vec<u8> {
    vec![1u8]
}

fn encode_governance_init_market_rewards(n: u64, epoch_slots: u64) -> Vec<u8> {
    let mut data = vec![2u8];
    data.extend_from_slice(&n.to_le_bytes());
    data.extend_from_slice(&epoch_slots.to_le_bytes());
    data
}

// ============================================================================
// Test environment
// ============================================================================

struct TestEnv {
    svm: LiteSVM,
    percolator_id: Pubkey,
    rewards_id: Pubkey,
    governance_id: Pubkey,
    payer: Keypair,
    dao_authority: Keypair,
    governance_authority_pda: Pubkey,
    slab: Pubkey,
    collateral_mint: Pubkey,
    vault: Pubkey,
    pyth_index: Pubkey,
    coin_mint: Pubkey,
    mint_authority_pda: Pubkey,
    account_count: u16,
}

impl TestEnv {
    fn new() -> Self {
        let mut svm = LiteSVM::new();

        let percolator_id = Pubkey::new_unique();
        let perc_bytes = std::fs::read(percolator_path()).expect("read percolator BPF");
        svm.add_program(percolator_id, &perc_bytes);

        let rewards_id = Pubkey::new_unique();
        let rewards_bytes = std::fs::read(rewards_path()).expect("read rewards BPF");
        svm.add_program(rewards_id, &rewards_bytes);

        let governance_id = governance_program_id();
        let governance_bytes = std::fs::read(governance_path()).expect("read governance BPF");
        svm.add_program(governance_id, &governance_bytes);

        let payer = Keypair::new();
        let slab = Pubkey::new_unique();
        let collateral_mint = Pubkey::new_unique();
        let pyth_index = Pubkey::new_unique();
        let (vault_pda, _) =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &percolator_id);
        let vault = Pubkey::new_unique();

        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

        // Slab account
        svm.set_account(
            slab,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; SLAB_LEN],
                owner: percolator_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // Collateral mint
        {
            let mut data = vec![0u8; Mint::LEN];
            let mint = Mint {
                mint_authority: solana_sdk::program_option::COption::None,
                supply: 0,
                decimals: 6,
                is_initialized: true,
                freeze_authority: solana_sdk::program_option::COption::None,
            };
            Mint::pack(mint, &mut data).unwrap();
            svm.set_account(
                collateral_mint,
                Account {
                    lamports: 1_000_000,
                    data,
                    owner: spl_token::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        }

        // Vault token account
        svm.set_account(
            vault,
            Account {
                lamports: 1_000_000,
                data: make_token_account_data(&collateral_mint, &vault_pda, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // Pyth price data: $100
        let pyth_data = make_pyth_data(&TEST_FEED_ID, 100_000_000, -6, 1, 100);
        svm.set_account(
            pyth_index,
            Account {
                lamports: 1_000_000,
                data: pyth_data,
                owner: PYTH_RECEIVER_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        // Set clock to slot 100
        svm.set_sysvar(&Clock {
            slot: 100,
            unix_timestamp: 100,
            ..Clock::default()
        });

        // DAO authority
        let dao_authority = Keypair::new();
        svm.airdrop(&dao_authority.pubkey(), 100_000_000_000)
            .unwrap();

        // COIN mint — authority is the rewards PDA derived from coin_mint key
        let coin_mint = Pubkey::new_unique();
        let (mint_authority_pda, _) = Pubkey::find_program_address(
            &[b"coin_mint_authority", coin_mint.as_ref()],
            &rewards_id,
        );
        let (governance_authority_pda, _) =
            governance_authority_address(&rewards_id, &coin_mint);
        svm.set_account(
            coin_mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_with_authority(&mint_authority_pda),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        let ix = Instruction {
            program_id: governance_id,
            accounts: vec![
                AccountMeta::new(dao_authority.pubkey(), true),
                AccountMeta::new(governance_authority_pda, false),
                AccountMeta::new_readonly(rewards_id, false),
                AccountMeta::new_readonly(coin_mint, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_governance_init_authority(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&dao_authority.pubkey()),
            &[&dao_authority],
            svm.latest_blockhash(),
        );
        svm.send_transaction(tx)
            .expect("governance init_authority failed");

        // Init percolator market
        let dummy_ata = Pubkey::new_unique();
        svm.set_account(
            dummy_ata,
            Account {
                lamports: 1_000_000,
                data: vec![0u8; TokenAccount::LEN],
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        let ix = Instruction {
            program_id: percolator_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(slab, false),
                AccountMeta::new_readonly(collateral_mint, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
                AccountMeta::new_readonly(sysvar::rent::ID, false),
                AccountMeta::new_readonly(dummy_ata, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market(
                &payer.pubkey(),
                &collateral_mint,
                &TEST_FEED_ID,
                0, // trading_fee_bps
            ),
        };
        let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
        let tx = Transaction::new_signed_with_payer(
            &[cu_ix, ix],
            Some(&payer.pubkey()),
            &[&payer],
            svm.latest_blockhash(),
        );
        svm.send_transaction(tx).expect("init_market failed");

        TestEnv {
            svm,
            percolator_id,
            rewards_id,
            governance_id,
            payer,
            dao_authority,
            governance_authority_pda,
            slab,
            collateral_mint,
            vault,
            pyth_index,
            coin_mint,
            mint_authority_pda,
            account_count: 0,
        }
    }

    fn init_coin_config(&mut self) {
        let coin_mint = self.coin_mint;
        self.try_init_coin_config_with_mint(&coin_mint)
            .expect("init_coin_config failed");
    }

    fn try_init_coin_config_direct_with_signers(
        &mut self,
        payer: &Keypair,
        authority: &Keypair,
        coin_mint: &Pubkey,
    ) -> Result<(), String> {
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", coin_mint.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(authority.pubkey(), true),
                AccountMeta::new_readonly(*coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_coin_config(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[payer, authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn try_init_coin_config_with_mint(&mut self, coin_mint: &Pubkey) -> Result<(), String> {
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", coin_mint.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(*coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_governance_init_coin_config(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn init_market_rewards(&mut self, n: u64, epoch_slots: u64) {
        let slab = self.slab;
        // Register MRC PDA as insurance_operator BEFORE admin burn.
        self.register_insurance_operator_for_slab(&slab);
        self.burn_market_admin();
        self.try_init_market_rewards_for_slab(&slab, n, epoch_slots)
            .expect("init_market_rewards failed");
    }

    fn try_init_market_rewards(
        &mut self,
        n: u64,
        epoch_slots: u64,
    ) -> Result<(), String> {
        let slab = self.slab;
        self.try_init_market_rewards_for_slab(&slab, n, epoch_slots)
    }

    fn try_init_market_rewards_for_slab(
        &mut self,
        slab: &Pubkey,
        n: u64,
        epoch_slots: u64,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", slab.as_ref()], &self.rewards_id);
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", self.coin_mint.as_ref()],
            &self.rewards_id,
        );
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", slab.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(*slab, false),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(self.collateral_mint, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::rent::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_governance_init_market_rewards(n, epoch_slots),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn burn_market_admin(&mut self) {
        let slab = self.slab;
        self.burn_market_admin_for_slab(&slab);
    }

    fn burn_market_admin_for_slab(&mut self, slab: &Pubkey) {
        let slab_account = self.svm.get_account(slab).expect("slab account missing");
        let header = percolator_prog::state::read_header(&slab_account.data);
        if header.admin == [0u8; 32] {
            return;
        }

        let admin = Keypair::from_bytes(&self.payer.to_bytes()).unwrap();
        // UpdateAuthority (tag 32): [current_signer, new_authority, slab]
        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new_readonly(Pubkey::default(), false),
                AccountMeta::new(*slab, false),
            ],
            data: encode_update_admin(&Pubkey::default()),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&admin.pubkey()),
            &[&admin],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("burn market admin failed");
    }

    /// Call our rewards program's register_insurance_operator instruction,
    /// which CPIs percolator's UpdateAuthority to set MRC_PDA as insurance_operator.
    /// Must be called BEFORE admin burn (admin signs here).
    fn register_insurance_operator_for_slab(&mut self, slab: &Pubkey) {
        let admin = Keypair::from_bytes(&self.payer.to_bytes()).unwrap();
        let (mrc_pda, _) = Pubkey::find_program_address(
            &[b"mrc", slab.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new_readonly(mrc_pda, false),
                AccountMeta::new(*slab, false),
                AccountMeta::new_readonly(self.percolator_id, false),
            ],
            data: vec![6u8], // IX_REGISTER_INSURANCE_OPERATOR
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&admin.pubkey()),
            &[&admin],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("register_insurance_operator failed");
    }

    fn stake(&mut self, user: &Keypair, amount: u64) {
        let result = self.try_stake(user, amount);
        result.expect("stake failed");
    }

    fn try_stake(&mut self, user: &Keypair, amount: u64) -> Result<(), String> {
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", self.slab.as_ref()],
            &self.rewards_id,
        );
        let (sp_pda, _) = Pubkey::find_program_address(
            &[b"sp", self.slab.as_ref(), user.pubkey().as_ref()],
            &self.rewards_id,
        );

        let col_mint = self.collateral_mint;
        let user_ata = self.create_ata(&col_mint, &user.pubkey(), amount);
        self.try_stake_with_accounts(user, amount, &user_ata, &stake_vault, &sp_pda, &spl_token::ID)
    }

    fn try_stake_with_accounts(
        &mut self,
        user: &Keypair,
        amount: u64,
        user_ata: &Pubkey,
        stake_vault: &Pubkey,
        sp_pda: &Pubkey,
        token_program: &Pubkey,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),           // [0] user
                AccountMeta::new(mrc_pda, false),                // [1] mrc
                AccountMeta::new_readonly(self.slab, false),     // [2] market_slab
                AccountMeta::new(*user_ata, false),              // [3] user_collateral_ata
                AccountMeta::new(*stake_vault, false),           // [4] stake_vault
                AccountMeta::new(*sp_pda, false),                // [5] stake_position
                AccountMeta::new_readonly(*token_program, false), // [6] token_program
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // [7] system
                AccountMeta::new_readonly(sysvar::clock::ID, false), // [8] clock
            ],
            data: encode_stake(amount),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&user.pubkey()),
            &[user],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn unstake(&mut self, user: &Keypair, amount: u64) {
        let result = self.try_unstake(user, amount);
        result.expect("unstake failed");
    }

    fn try_unstake(&mut self, user: &Keypair, amount: u64) -> Result<(), String> {
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", self.slab.as_ref()],
            &self.rewards_id,
        );
        let (sp_pda, _) = Pubkey::find_program_address(
            &[b"sp", self.slab.as_ref(), user.pubkey().as_ref()],
            &self.rewards_id,
        );

        let col_mint = self.collateral_mint;
        let user_ata = self.create_ata(&col_mint, &user.pubkey(), 0);
        let coin_ata = self.create_coin_ata(&user.pubkey(), 0);
        self.try_unstake_with_accounts(user, amount, &user_ata, &coin_ata, &stake_vault, &sp_pda)
    }

    fn try_unstake_with_accounts(
        &mut self,
        user: &Keypair,
        amount: u64,
        user_ata: &Pubkey,
        coin_ata: &Pubkey,
        stake_vault: &Pubkey,
        sp_pda: &Pubkey,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),               // [0] user
                AccountMeta::new(mrc_pda, false),                    // [1] mrc
                AccountMeta::new_readonly(self.slab, false),         // [2] market_slab
                AccountMeta::new(*user_ata, false),                  // [3] user_collateral_ata
                AccountMeta::new(*stake_vault, false),               // [4] stake_vault
                AccountMeta::new(*sp_pda, false),                    // [5] stake_position
                AccountMeta::new(self.coin_mint, false),             // [6] coin_mint
                AccountMeta::new(*coin_ata, false),                  // [7] user_coin_ata
                AccountMeta::new_readonly(self.mint_authority_pda, false), // [8] mint_authority
                AccountMeta::new_readonly(spl_token::ID, false),     // [9] token_program
                AccountMeta::new_readonly(sysvar::clock::ID, false), // [10] clock
            ],
            data: encode_unstake(amount),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&user.pubkey()),
            &[user],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    /// Unstake and return the (collateral_ata, coin_ata) for balance checking
    fn unstake_and_get_atas(&mut self, user: &Keypair, amount: u64) -> (Pubkey, Pubkey) {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", self.slab.as_ref()],
            &self.rewards_id,
        );
        let (sp_pda, _) = Pubkey::find_program_address(
            &[b"sp", self.slab.as_ref(), user.pubkey().as_ref()],
            &self.rewards_id,
        );

        let col_mint = self.collateral_mint;
        let user_ata = self.create_ata(&col_mint, &user.pubkey(), 0);
        let coin_ata = self.create_coin_ata(&user.pubkey(), 0);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(user_ata, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new(sp_pda, false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new(coin_ata, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_unstake(amount),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&user.pubkey()),
            &[user],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).expect("unstake failed");
        (user_ata, coin_ata)
    }

    fn claim_stake_rewards(&mut self, user: &Keypair) -> Pubkey {
        let coin_ata = self.create_coin_ata(&user.pubkey(), 0);
        self.claim_stake_rewards_to(user, &coin_ata);
        coin_ata
    }

    fn claim_stake_rewards_to(&mut self, user: &Keypair, coin_ata: &Pubkey) {
        let result = self.try_claim_stake_rewards_to(user, coin_ata);
        result.expect("claim_stake_rewards failed");
    }

    fn try_claim_stake_rewards_to(
        &mut self,
        user: &Keypair,
        coin_ata: &Pubkey,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (sp_pda, _) = Pubkey::find_program_address(
            &[b"sp", self.slab.as_ref(), user.pubkey().as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),               // [0] user
                AccountMeta::new(mrc_pda, false),                    // [1] mrc
                AccountMeta::new_readonly(self.slab, false),         // [2] market_slab
                AccountMeta::new(sp_pda, false),                     // [3] stake_position
                AccountMeta::new(self.coin_mint, false),             // [4] coin_mint
                AccountMeta::new(*coin_ata, false),                  // [5] user_coin_ata
                AccountMeta::new_readonly(self.mint_authority_pda, false), // [6] mint_authority
                AccountMeta::new_readonly(spl_token::ID, false),     // [7] token_program
                AccountMeta::new_readonly(sysvar::clock::ID, false), // [8] clock
            ],
            data: encode_claim_stake_rewards(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&user.pubkey()),
            &[user],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn init_lp(&mut self, owner: &Keypair) -> u16 {
        let idx = self.account_count;
        self.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let col_mint = self.collateral_mint;
        let ata = self.create_ata(&col_mint, &owner.pubkey(), 0);
        let matcher = spl_token::ID;
        let ctx = Pubkey::new_unique();
        self.svm
            .set_account(
                ctx,
                Account {
                    lamports: 1_000_000,
                    data: vec![0u8; 320],
                    owner: matcher,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();

        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.slab, false),
                AccountMeta::new(ata, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(matcher, false),
                AccountMeta::new_readonly(ctx, false),
            ],
            data: encode_init_lp(&matcher, &ctx, 0),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&owner.pubkey()),
            &[owner],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).expect("init_lp failed");
        self.account_count += 1;
        idx
    }

    fn init_user(&mut self, owner: &Keypair) -> u16 {
        let idx = self.account_count;
        self.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let col_mint = self.collateral_mint;
        let _ata = self.create_ata(&col_mint, &owner.pubkey(), 1_000_000);

        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.slab, false),
                AccountMeta::new(_ata, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_init_user(1_000_000),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&owner.pubkey()),
            &[owner],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).expect("init_user failed");
        self.account_count += 1;
        idx
    }

    fn deposit(&mut self, owner: &Keypair, idx: u16, amount: u64) {
        let col_mint = self.collateral_mint;
        let ata = self.create_ata(&col_mint, &owner.pubkey(), amount);
        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.slab, false),
                AccountMeta::new(ata, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_deposit(idx, amount),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&owner.pubkey()),
            &[owner],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).expect("deposit failed");
    }

    fn trade(&mut self, user: &Keypair, lp: &Keypair, lp_idx: u16, user_idx: u16, size: i128) {
        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new(lp.pubkey(), true),
                AccountMeta::new(self.slab, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
                AccountMeta::new_readonly(self.pyth_index, false),
            ],
            data: encode_trade(lp_idx, user_idx, size),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&user.pubkey()),
            &[user, lp],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).expect("trade failed");
    }

    fn create_ata(&mut self, mint: &Pubkey, owner: &Pubkey, amount: u64) -> Pubkey {
        let ata = Pubkey::new_unique();
        self.svm
            .set_account(
                ata,
                Account {
                    lamports: 1_000_000,
                    data: make_token_account_data(mint, owner, amount),
                    owner: spl_token::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        ata
    }

    fn create_coin_ata(&mut self, owner: &Pubkey, amount: u64) -> Pubkey {
        let mint = self.coin_mint;
        self.create_ata(&mint, owner, amount)
    }

    fn set_clock(&mut self, slot: u64) {
        self.svm.set_sysvar(&Clock {
            slot,
            unix_timestamp: slot as i64,
            ..Clock::default()
        });
        self.svm.expire_blockhash();
    }

    fn advance_blockhash(&mut self) {
        self.svm.expire_blockhash();
    }

    fn read_token_balance(&self, account: &Pubkey) -> u64 {
        let data = self.svm.get_account(account).unwrap();
        let token = TokenAccount::unpack(&data.data).unwrap();
        token.amount
    }

    fn vault_balance(&self) -> u64 {
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", self.slab.as_ref()],
            &self.rewards_id,
        );
        self.read_token_balance(&stake_vault)
    }

    fn draw_insurance(&mut self, amount: u64, destination: &Pubkey) {
        self.try_draw_insurance(amount, destination)
            .expect("draw_insurance failed");
    }

    fn try_draw_insurance(&mut self, amount: u64, destination: &Pubkey) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", self.coin_mint.as_ref()],
            &self.rewards_id,
        );
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", self.slab.as_ref()],
            &self.rewards_id,
        );

        let mut data = vec![3u8]; // governance adapter IX_DRAW_INSURANCE
        data.extend_from_slice(&amount.to_le_bytes());

        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(mrc_pda, false),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new(*destination, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn try_draw_insurance_direct(
        &mut self,
        signer: &Keypair,
        amount: u64,
        destination: &Pubkey,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", self.coin_mint.as_ref()],
            &self.rewards_id,
        );
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", self.slab.as_ref()],
            &self.rewards_id,
        );

        let mut data = vec![5u8]; // rewards program IX_DRAW_INSURANCE directly
        data.extend_from_slice(&amount.to_le_bytes());

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new_readonly(signer.pubkey(), true),
                AccountMeta::new_readonly(mrc_pda, false),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new(*destination, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&signer.pubkey()),
            &[signer],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    // ========================================================================
    // Multi-market helpers
    // ========================================================================

    /// Create a second percolator market sharing the same COIN. Returns the slab key.
    fn init_second_market(&mut self, n_per_epoch: u64, epoch_slots: u64) -> Pubkey {
        let slab2 = Pubkey::new_unique();
        let vault2 = Pubkey::new_unique();
        let (vault2_pda, _) = Pubkey::find_program_address(
            &[b"vault", slab2.as_ref()], &self.percolator_id);
        self.svm.set_account(slab2, Account {
            lamports: 1_000_000_000,
            data: vec![0u8; SLAB_LEN],
            owner: self.percolator_id,
            executable: false,
            rent_epoch: 0,
        }).unwrap();
        self.svm.set_account(vault2, Account {
            lamports: 1_000_000,
            data: make_token_account_data(&self.collateral_mint, &vault2_pda, 0),
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        }).unwrap();
        let dummy_ata2 = Pubkey::new_unique();
        self.svm.set_account(dummy_ata2, Account {
            lamports: 1_000_000,
            data: vec![0u8; TokenAccount::LEN],
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        }).unwrap();
        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new(slab2, false),
                AccountMeta::new_readonly(self.collateral_mint, false),
                AccountMeta::new(vault2, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
                AccountMeta::new_readonly(sysvar::rent::ID, false),
                AccountMeta::new_readonly(dummy_ata2, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market(
                &self.payer.pubkey(), &self.collateral_mint, &TEST_FEED_ID, 0),
        };
        let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
        let tx = Transaction::new_signed_with_payer(
            &[cu_ix, ix], Some(&self.payer.pubkey()),
            &[&self.payer], self.svm.latest_blockhash());
        self.svm.send_transaction(tx).expect("init_second_market failed");
        self.register_insurance_operator_for_slab(&slab2);
        self.burn_market_admin_for_slab(&slab2);
        self.try_init_market_rewards_for_slab(&slab2, n_per_epoch, epoch_slots)
            .expect("init_market_rewards for second market failed");
        slab2
    }

    fn vault_balance_for(&self, slab: &Pubkey) -> u64 {
        let (vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", slab.as_ref()], &self.rewards_id);
        self.read_token_balance(&vault)
    }

    /// Stake into a specific market.
    fn stake_in(&mut self, slab: &Pubkey, user: &Keypair, amount: u64) {
        let (mrc_pda, _) = Pubkey::find_program_address(
            &[b"mrc", slab.as_ref()], &self.rewards_id);
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", slab.as_ref()], &self.rewards_id);
        let (sp_pda, _) = Pubkey::find_program_address(
            &[b"sp", slab.as_ref(), user.pubkey().as_ref()], &self.rewards_id);
        let col_mint = self.collateral_mint;
        let user_ata = self.create_ata(&col_mint, &user.pubkey(), amount);
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(*slab, false),
                AccountMeta::new(user_ata, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new(sp_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_stake(amount),
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix], Some(&user.pubkey()),
            &[user], self.svm.latest_blockhash());
        self.svm.send_transaction(tx).expect("stake_in failed");
    }

    /// Unstake from a specific market. Returns (collateral_ata, coin_ata).
    fn unstake_in_get_atas(&mut self, slab: &Pubkey, user: &Keypair, amount: u64) -> (Pubkey, Pubkey) {
        let (mrc_pda, _) = Pubkey::find_program_address(
            &[b"mrc", slab.as_ref()], &self.rewards_id);
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", slab.as_ref()], &self.rewards_id);
        let (sp_pda, _) = Pubkey::find_program_address(
            &[b"sp", slab.as_ref(), user.pubkey().as_ref()], &self.rewards_id);
        let col_mint = self.collateral_mint;
        let user_ata = self.create_ata(&col_mint, &user.pubkey(), 0);
        let coin_ata = self.create_coin_ata(&user.pubkey(), 0);
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(*slab, false),
                AccountMeta::new(user_ata, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new(sp_pda, false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new(coin_ata, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_unstake(amount),
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix], Some(&user.pubkey()),
            &[user], self.svm.latest_blockhash());
        self.svm.send_transaction(tx).expect("unstake_in failed");
        (user_ata, coin_ata)
    }

    /// Draw insurance profit from a specific market.
    fn draw_insurance_from(&mut self, slab: &Pubkey, amount: u64, destination: &Pubkey) {
        self.try_draw_insurance_from(slab, amount, destination)
            .expect("draw_insurance_from failed");
    }

    fn try_draw_insurance_from(
        &mut self, slab: &Pubkey, amount: u64, destination: &Pubkey,
    ) -> Result<(), String> {
        let (mrc_pda, _) = Pubkey::find_program_address(
            &[b"mrc", slab.as_ref()], &self.rewards_id);
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", self.coin_mint.as_ref()], &self.rewards_id);
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", slab.as_ref()], &self.rewards_id);

        let mut data = vec![3u8]; // governance adapter IX_DRAW_INSURANCE
        data.extend_from_slice(&amount.to_le_bytes());

        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(mrc_pda, false),
                AccountMeta::new_readonly(*slab, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new(*destination, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix], Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority], self.svm.latest_blockhash());
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    /// Inject "profit" into a vault by sending tokens directly (bypasses stake).
    fn inject_profit(&mut self, slab: &Pubkey, amount: u64) {
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", slab.as_ref()], &self.rewards_id);
        let col_mint = self.collateral_mint;
        let donor_ata = self.create_ata(&col_mint, &self.dao_authority.pubkey(), amount);
        let xfer = spl_token::instruction::transfer(
            &spl_token::ID, &donor_ata, &stake_vault,
            &self.dao_authority.pubkey(), &[], amount,
        ).unwrap();
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[xfer], Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority], self.svm.latest_blockhash());
        self.svm.send_transaction(tx).expect("inject_profit failed");
    }

    /// Top up percolator's insurance fund directly via percolator::TopUpInsurance
    /// (permissionless, any donor can call). Simulates fees/profit accruing in
    /// the percolator market's insurance fund.
    fn topup_percolator_insurance(&mut self, slab: &Pubkey, amount: u64) {
        let donor = Keypair::new();
        self.svm.airdrop(&donor.pubkey(), 1_000_000_000).unwrap();
        let col_mint = self.collateral_mint;
        let donor_ata = self.create_ata(&col_mint, &donor.pubkey(), amount);

        // The percolator vault for a given slab is stored in MarketConfig.
        // For the default env.slab we use env.vault; for new slabs we look up.
        let perc_vault = if *slab == self.slab {
            self.vault
        } else {
            // Need to get from MarketConfig — stored at known offset in slab.
            let slab_data = self.svm.get_account(slab).unwrap().data;
            let config = percolator_prog::state::read_config(&slab_data);
            Pubkey::new_from_array(config.vault_pubkey)
        };

        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(donor.pubkey(), true),
                AccountMeta::new(*slab, false),
                AccountMeta::new(donor_ata, false),
                AccountMeta::new(perc_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_topup_insurance(amount),
        };
        let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(400_000);
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[cu_ix, ix], Some(&donor.pubkey()),
            &[&donor], self.svm.latest_blockhash());
        self.svm.send_transaction(tx).expect("topup_percolator_insurance failed");
    }

    /// Call our rewards program's pull_insurance instruction, which CPIs
    /// WithdrawInsuranceLimited on percolator, destination = our stake_vault.
    /// Returns Ok(()) on success, Err on failure (e.g., cooldown not elapsed,
    /// insufficient balance, MRC PDA not operator).
    fn try_pull_insurance(&mut self, slab: &Pubkey, amount: u64) -> Result<(), String> {
        let (mrc_pda, _) = Pubkey::find_program_address(
            &[b"mrc", slab.as_ref()], &self.rewards_id);
        let (stake_vault, _) = Pubkey::find_program_address(
            &[b"stake_vault", slab.as_ref()], &self.rewards_id);
        let (perc_vault_pda, _) = Pubkey::find_program_address(
            &[b"vault", slab.as_ref()], &self.percolator_id);

        let perc_vault = if *slab == self.slab {
            self.vault
        } else {
            let slab_data = self.svm.get_account(slab).unwrap().data;
            let config = percolator_prog::state::read_config(&slab_data);
            Pubkey::new_from_array(config.vault_pubkey)
        };

        let mut data = vec![7u8]; // IX_PULL_INSURANCE
        data.extend_from_slice(&amount.to_le_bytes());

        let payer = Keypair::new();
        self.svm.airdrop(&payer.pubkey(), 1_000_000_000).unwrap();

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(mrc_pda, false),
                AccountMeta::new(*slab, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new(perc_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(perc_vault_pda, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
                AccountMeta::new_readonly(self.percolator_id, false),
            ],
            data,
        };
        let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(400_000);
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[cu_ix, ix], Some(&payer.pubkey()),
            &[&payer], self.svm.latest_blockhash());
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn pull_insurance(&mut self, slab: &Pubkey, amount: u64) {
        self.try_pull_insurance(slab, amount)
            .expect("pull_insurance failed");
    }

    /// Read the current percolator insurance fund balance from the slab's engine state.
    /// Uses BPF-target byte offset (ENGINE_OFF=568, insurance_fund.balance at +16).
    fn percolator_insurance_balance(&self, slab: &Pubkey) -> u128 {
        const INSURANCE_BALANCE_OFFSET: usize = 568 + 16;
        let slab_data = self.svm.get_account(slab).unwrap().data;
        u128::from_le_bytes(
            slab_data[INSURANCE_BALANCE_OFFSET..INSURANCE_BALANCE_OFFSET + 16]
                .try_into()
                .unwrap(),
        )
    }
}

// ============================================================================
// Tests: init_coin_config
// ============================================================================

#[test]
fn test_init_coin_config_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let (coin_cfg_pda, _) =
        Pubkey::find_program_address(&[b"coin_cfg", env.coin_mint.as_ref()], &env.rewards_id);
    let cfg_account = env.svm.get_account(&coin_cfg_pda).unwrap();
    assert_eq!(cfg_account.owner, env.rewards_id);
    assert_eq!(cfg_account.data.len(), 40); // COIN_CFG_SIZE = 8 + 32

    assert_eq!(&cfg_account.data[..8], b"CCFG_INI");
    let stored_auth = Pubkey::new_from_array(cfg_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_auth, env.governance_authority_pda);
}

#[test]
fn test_init_coin_config_double_init_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    env.advance_blockhash();
    let mint = env.coin_mint;
    let result = env.try_init_coin_config_with_mint(&mint);
    assert!(result.is_err(), "Double init should fail");
}

#[test]
fn test_init_coin_config_direct_eoa_authority_rejected() {
    let mut env = TestEnv::new();
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    let mint = env.coin_mint;
    let result = env.try_init_coin_config_direct_with_signers(&attacker, &attacker, &mint);
    assert!(result.is_err(), "Direct EOA authority should be rejected");
}

#[test]
fn test_init_coin_config_wrong_mint_authority_fails() {
    let mut env = TestEnv::new();

    let wrong_auth = Pubkey::new_unique();
    let mint = env.coin_mint;
    env.svm
        .set_account(
            mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_with_authority(&wrong_auth),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_coin_config_with_mint(&mint);
    assert!(result.is_err(), "Wrong mint_authority should fail");
}

#[test]
fn test_init_coin_config_freeze_authority_fails() {
    let mut env = TestEnv::new();
    let freeze = Pubkey::new_unique();

    let mint = env.coin_mint;
    let ma = env.mint_authority_pda;
    env.svm
        .set_account(
            mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_with_freeze(&ma, &freeze),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_coin_config_with_mint(&mint);
    assert!(result.is_err(), "Mint with freeze_authority should fail");
}

#[test]
fn test_init_coin_config_no_mint_authority_fails() {
    let mut env = TestEnv::new();

    let mint = env.coin_mint;
    env.svm
        .set_account(
            mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_no_authority(),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_coin_config_with_mint(&mint);
    assert!(result.is_err(), "Mint with no authority should fail");
}

// ============================================================================
// Tests: init_market_rewards
// ============================================================================

#[test]
fn test_init_market_rewards_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let n = 1000u64;
    let epoch_slots = 216_000u64;

    env.init_market_rewards(n, epoch_slots);

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let mrc_account = env.svm.get_account(&mrc_pda).unwrap();
    assert_eq!(mrc_account.owner, env.rewards_id);
    assert_eq!(mrc_account.data.len(), 160); // MRC_SIZE

    assert_eq!(&mrc_account.data[..8], b"MRC_V003");

    let stored_slab = Pubkey::new_from_array(mrc_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_slab, env.slab);

    let stored_mint = Pubkey::new_from_array(mrc_account.data[40..72].try_into().unwrap());
    assert_eq!(stored_mint, env.coin_mint);

    let stored_collateral =
        Pubkey::new_from_array(mrc_account.data[72..104].try_into().unwrap());
    assert_eq!(stored_collateral, env.collateral_mint);

    let stored_n = u64::from_le_bytes(mrc_account.data[104..112].try_into().unwrap());
    assert_eq!(stored_n, n);

    let stored_epoch_slots = u64::from_le_bytes(mrc_account.data[112..120].try_into().unwrap());
    assert_eq!(stored_epoch_slots, epoch_slots);

    let stored_start = u64::from_le_bytes(mrc_account.data[120..128].try_into().unwrap());
    assert_eq!(stored_start, 100); // clock was set to 100 during init

    // Verify stake vault was created
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()],
        &env.rewards_id,
    );
    let vault_account = env.svm.get_account(&stake_vault).unwrap();
    assert_eq!(vault_account.owner, spl_token::ID);
    let vault_token = TokenAccount::unpack(&vault_account.data).unwrap();
    assert_eq!(vault_token.mint, env.collateral_mint);
    // vault authority = mrc PDA
    assert_eq!(vault_token.owner, mrc_pda);
}

#[test]
fn test_init_market_rewards_live_admin_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let result = env.try_init_market_rewards(1000, 216_000);
    assert!(result.is_err(), "Live-admin slab should be rejected");
}

#[test]
fn test_trusted_bootstrap_ceremony_flow() {
    let mut env = TestEnv::new();

    // Step 0: the DAO-controlled client bootstraps the governance authority path.
    let governance_authority = env
        .svm
        .get_account(&env.governance_authority_pda)
        .expect("governance authority PDA should exist");
    assert_eq!(governance_authority.owner, env.governance_id);

    // Step 1: initialize CoinConfig through that governed path.
    env.init_coin_config();
    let (coin_cfg_pda, _) = Pubkey::find_program_address(
        &[b"coin_cfg", env.coin_mint.as_ref()],
        &env.rewards_id,
    );
    let coin_cfg = env.svm.get_account(&coin_cfg_pda).expect("coin config must exist");
    let stored_auth = Pubkey::new_from_array(coin_cfg.data[8..40].try_into().unwrap());
    assert_eq!(stored_auth, env.governance_authority_pda);

    // Step 2: reward init is blocked until Percolator admin is burned.
    let live_header =
        percolator_prog::state::read_header(&env.svm.get_account(&env.slab).unwrap().data);
    assert_eq!(Pubkey::new_from_array(live_header.admin), env.payer.pubkey());
    let result = env.try_init_market_rewards(1000, 216_000);
    assert!(result.is_err(), "live-admin slab should be rejected before burn");
    env.advance_blockhash();

    // Step 3: register MRC PDA as insurance_operator, then burn admin.
    env.register_insurance_operator_for_slab(&env.slab.clone());
    env.burn_market_admin();
    let burned_header =
        percolator_prog::state::read_header(&env.svm.get_account(&env.slab).unwrap().data);
    assert_eq!(burned_header.admin, [0u8; 32], "admin must be burned");
    env.advance_blockhash();

    // Step 4: reward init now succeeds through the same governed path.
    let slab = env.slab;
    env.try_init_market_rewards_for_slab(&slab, 1000, 216_000)
        .expect("reward init should succeed after admin burn");
    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let mrc = env.svm.get_account(&mrc_pda).expect("market rewards config must exist");
    assert_eq!(mrc.owner, env.rewards_id);
    let stored_start = u64::from_le_bytes(mrc.data[120..128].try_into().unwrap());
    assert_eq!(stored_start, 100);
}

#[test]
fn test_init_market_rewards_double_init_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 216_000);

    env.advance_blockhash();
    let result = env.try_init_market_rewards(1000, 216_000);
    assert!(result.is_err(), "Double init should fail");
}

#[test]
fn test_init_market_rewards_epoch_slots_zero_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let result = env.try_init_market_rewards(1000, 0);
    assert!(result.is_err(), "epoch_slots = 0 should fail");
}

#[test]
fn test_init_market_rewards_wrong_authority_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (coin_cfg_pda, _) = Pubkey::find_program_address(
        &[b"coin_cfg", env.coin_mint.as_ref()],
        &env.rewards_id,
    );
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()],
        &env.rewards_id,
    );

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new_readonly(attacker.pubkey(), true),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(env.collateral_mint, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(1000, 216_000),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&attacker.pubkey()),
        &[&attacker],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Wrong authority should be rejected");
}

// ============================================================================
// Tests: stake
// ============================================================================

#[test]
fn test_stake_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100); // N=1000, K=0, epoch_slots=100

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);

    // Verify StakePosition PDA was created
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let sp_account = env.svm.get_account(&sp_pda).unwrap();
    assert_eq!(sp_account.owner, env.rewards_id);
    assert_eq!(sp_account.data.len(), 48); // SP_SIZE

    assert_eq!(&sp_account.data[..8], b"SP__INIT");
    let amount = u64::from_le_bytes(sp_account.data[8..16].try_into().unwrap());
    assert_eq!(amount, 1_000_000);

    // Verify vault received collateral
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()],
        &env.rewards_id,
    );
    let vault_balance = env.read_token_balance(&stake_vault);
    assert_eq!(vault_balance, 1_000_000);

    // Verify MRC total_staked updated
    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let mrc_data = env.svm.get_account(&mrc_pda).unwrap();
    let total_staked = u64::from_le_bytes(mrc_data.data[152..160].try_into().unwrap());
    assert_eq!(total_staked, 1_000_000);
}

#[test]
fn test_stake_zero_amount_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    let result = env.try_stake(&user, 0);
    assert!(result.is_err(), "Staking 0 should fail");
}

#[test]
fn test_stake_wrong_token_program_fails_with_incorrect_program_id() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()],
        &env.rewards_id,
    );
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let collateral_mint = env.collateral_mint;
    let user_ata = env.create_ata(&collateral_mint, &user.pubkey(), 500);
    let fake_token_program = Pubkey::new_unique();

    let result = env.try_stake_with_accounts(
        &user,
        500,
        &user_ata,
        &stake_vault,
        &sp_pda,
        &fake_token_program,
    );
    assert!(result.is_err(), "Stake must reject a non-SPL token program");
    let err = result.unwrap_err();
    assert!(
        err.contains("IncorrectProgramId"),
        "Expected IncorrectProgramId, got {err}"
    );
}

#[test]
fn test_additional_deposit_increases_balance() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // First deposit at slot 100
    env.stake(&user, 500_000);

    // Advance to slot 150
    env.set_clock(150);
    // Deposit more
    env.stake(&user, 300_000);

    // Verify total amount
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let sp_data = env.svm.get_account(&sp_pda).unwrap();
    let amount = u64::from_le_bytes(sp_data.data[8..16].try_into().unwrap());
    assert_eq!(amount, 800_000);
}

// ============================================================================
// Tests: withdraw (unstake)
// ============================================================================

#[test]
fn test_withdraw_returns_collateral_and_rewards() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let epoch_slots = 100u64;
    env.init_market_rewards(1000, epoch_slots); // N=1000/epoch

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // Deposit at slot 100
    env.stake(&user, 1_000_000);

    // Advance 1 epoch
    env.set_clock(200);

    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);

    // Collateral returned
    let col_balance = env.read_token_balance(&col_ata);
    assert_eq!(col_balance, 1_000_000, "Should get collateral back");

    // COIN rewards minted: 1000 * (200-100) / 100 = ~1000 for 1 epoch elapsed
    // (integer truncation may lose up to 1 COIN)
    let coin_balance = env.read_token_balance(&coin_ata);
    assert!(coin_balance >= 999 && coin_balance <= 1000, "Should get ~1000 COIN for 1 epoch, got {}", coin_balance);
}


#[test]
fn test_unstake_more_than_staked_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 500_000);
    env.set_clock(200);

    let result = env.try_unstake(&user, 1_000_000);
    assert!(result.is_err(), "Cannot unstake more than staked");
}

#[test]
fn test_partial_unstake() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let epoch_slots = 100u64;
    env.init_market_rewards(1000, epoch_slots);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);
    env.set_clock(200); // 1 epoch elapsed

    // Unstake half
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 500_000);
    let col_balance = env.read_token_balance(&col_ata);
    assert_eq!(col_balance, 500_000);

    let coin_balance = env.read_token_balance(&coin_ata);
    assert!(coin_balance >= 999 && coin_balance <= 1000, "Full pending rewards ~1000, got {}", coin_balance);

    // Verify position still exists with remaining amount
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let sp_data = env.svm.get_account(&sp_pda).unwrap();
    let remaining = u64::from_le_bytes(sp_data.data[8..16].try_into().unwrap());
    assert_eq!(remaining, 500_000);
}

#[test]
fn test_unstake_wrong_owner_destinations_fail() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    let attacker = Pubkey::new_unique();
    let collateral_mint = env.collateral_mint;
    let attacker_col_ata = env.create_ata(&collateral_mint, &attacker, 0);
    let attacker_coin_ata = env.create_coin_ata(&attacker, 0);
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()],
        &env.rewards_id,
    );
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );

    env.set_clock(200);
    let result = env.try_unstake_with_accounts(
        &user,
        1_000_000,
        &attacker_col_ata,
        &attacker_coin_ata,
        &stake_vault,
        &sp_pda,
    );
    assert!(
        result.is_err(),
        "Unstake must reject collateral or reward destinations not owned by the staker"
    );
}

#[test]
fn test_full_unstake_closes_position() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);
    env.set_clock(200);

    env.unstake(&user, 1_000_000);

    // Position PDA should be zeroed out (closed)
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let sp_account = env.svm.get_account(&sp_pda);
    // Account should be gone (zero lamports, empty data)
    match sp_account {
        Some(acct) => assert_eq!(acct.lamports, 0, "Position account should have 0 lamports"),
        None => {} // also fine — account was deleted
    }
}

// ============================================================================
// Tests: claim rewards (without withdrawing)
// ============================================================================

#[test]
fn test_claim_rewards_without_withdrawing() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);

    // Advance half an epoch — claim works at any time
    env.set_clock(150);
    let coin_ata = env.claim_stake_rewards(&user);

    // 1000 * 50 / 100 = ~500 COIN (integer truncation may lose 1)
    let balance = env.read_token_balance(&coin_ata);
    assert!(balance >= 499 && balance <= 500, "Should earn ~500 COIN for half epoch, got {}", balance);
}

#[test]
fn test_claim_stake_rewards_multiple_times() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);

    // Claim at slot 200 (1 epoch)
    env.set_clock(200);
    let coin_ata = env.create_coin_ata(&user.pubkey(), 0);
    env.claim_stake_rewards_to(&user, &coin_ata);
    let bal1 = env.read_token_balance(&coin_ata);
    assert!(bal1 >= 999 && bal1 <= 1000, "~1000 for 1 epoch, got {}", bal1);

    // Claim again in same slot — should get 0 more
    env.advance_blockhash();
    env.claim_stake_rewards_to(&user, &coin_ata);
    assert_eq!(env.read_token_balance(&coin_ata), bal1, "No extra in same slot");

    // Advance to slot 400 (3 total epochs from start)
    env.set_clock(400);
    env.claim_stake_rewards_to(&user, &coin_ata);
    let bal3 = env.read_token_balance(&coin_ata);
    assert!(bal3 >= 2997 && bal3 <= 3000, "~3000 for 3 epochs, got {}", bal3);
}

#[test]
fn test_claim_stake_rewards_wrong_owner_destination_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    let attacker = Pubkey::new_unique();
    let attacker_coin_ata = env.create_coin_ata(&attacker, 0);

    env.set_clock(200);
    let result = env.try_claim_stake_rewards_to(&user, &attacker_coin_ata);
    assert!(
        result.is_err(),
        "Claim must reject a COIN destination not owned by the staker"
    );
}

#[test]
fn test_claim_stake_rewards_zero_at_start() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);

    // Claim immediately (same slot as stake) — should get 0
    let coin_ata = env.claim_stake_rewards(&user);
    assert_eq!(env.read_token_balance(&coin_ata), 0);
}

// ============================================================================
// Tests: multi-user deposit accumulator
// ============================================================================

#[test]
fn test_two_users_equal_stake() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100); // N=1000/epoch, epoch_slots=100

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Both stake equal amounts at same time
    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);

    // Advance 1 epoch
    env.set_clock(200);

    let ata_a = env.claim_stake_rewards(&alice);
    let ata_b = env.claim_stake_rewards(&bob);

    let bal_a = env.read_token_balance(&ata_a);
    let bal_b = env.read_token_balance(&ata_b);

    // Each gets half: 1000 * 1 / 2 = ~500 (rounding: up to 1 token lost per user)
    assert!(bal_a >= 499 && bal_a <= 500, "Alice gets ~50%, got {}", bal_a);
    assert!(bal_b >= 499 && bal_b <= 500, "Bob gets ~50%, got {}", bal_b);
    // Conservation: total emitted must equal ~1000 (1 epoch).
    // Each user may lose up to 1 token to fixed-point truncation.
    let total = bal_a + bal_b;
    assert!(total >= 998 && total <= 1000, "Total COIN must be ~1000, got {}", total);
}

#[test]
fn test_two_users_different_amounts() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Alice stakes 3x, Bob stakes 1x
    env.stake(&alice, 3_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);

    env.set_clock(200); // 1 epoch

    let ata_a = env.claim_stake_rewards(&alice);
    let ata_b = env.claim_stake_rewards(&bob);

    let bal_a = env.read_token_balance(&ata_a);
    let bal_b = env.read_token_balance(&ata_b);

    // Alice: 1000 * 3M / 4M = ~750, Bob: 1000 * 1M / 4M = ~250
    assert!(bal_a >= 749 && bal_a <= 750, "Alice gets ~75%, got {}", bal_a);
    assert!(bal_b >= 249 && bal_b <= 250, "Bob gets ~25%, got {}", bal_b);
    // Conservation: total emitted ≤ 1000 (1 epoch), lost up to 1 per user
    let total = bal_a + bal_b;
    assert!(total >= 998 && total <= 1000, "Total COIN must be ~1000, got {}", total);
}

#[test]
fn test_staker_joins_later() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Alice stakes at slot 100 (market start)
    env.stake(&alice, 1_000_000);

    // Advance 1 epoch; Alice earns all rewards for this period
    env.set_clock(200);

    // Bob joins at slot 200
    env.stake(&bob, 1_000_000);

    // Advance another epoch to slot 300
    env.set_clock(300);

    let ata_a = env.claim_stake_rewards(&alice);
    let ata_b = env.claim_stake_rewards(&bob);

    let bal_a = env.read_token_balance(&ata_a);
    let bal_b = env.read_token_balance(&ata_b);

    // Alice: epoch [100..200] alone = ~1000, epoch [200..300] shared = ~500 => ~1500
    // Bob: epoch [200..300] shared = ~500
    assert!(bal_a >= 1498 && bal_a <= 1500, "Alice: ~1500, got {}", bal_a);
    assert!(bal_b >= 499 && bal_b <= 500, "Bob: ~500, got {}", bal_b);
    // Conservation: 2 epochs elapsed = ~2000 total COIN (up to 1 per user truncation)
    let total = bal_a + bal_b;
    assert!(total >= 1996 && total <= 2000, "Total COIN must be ~2000, got {}", total);
}

#[test]
fn test_staker_leaves_then_another_earns_all() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Both stake at slot 100
    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);

    // Advance 1 epoch
    env.set_clock(200);

    // Alice withdraws (full)
    env.unstake(&alice, 1_000_000);

    // Advance another epoch
    env.set_clock(300);

    // Bob claims — should get all rewards for [200..300] alone
    let ata_b = env.claim_stake_rewards(&bob);
    let bal_b = env.read_token_balance(&ata_b);

    // epoch [100..200]: ~1000 / 2 = ~500 each
    // epoch [200..300]: ~1000 all to Bob
    // Bob total: ~500 + ~1000 = ~1500
    assert!(bal_b >= 1498 && bal_b <= 1500, "Bob: ~1500, got {}", bal_b);
}

// ============================================================================
// Tests: multi-user staggered withdrawal (insurance depositors exit at different times)
// ============================================================================

#[test]
fn test_two_users_unstake_at_different_times() {
    // Alice and Bob both stake 1M at slot 100.
    // Alice unstakes at slot 200 (1 epoch). Bob unstakes at slot 300 (2 epochs).
    // Verify both collateral returns and COIN reward amounts.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100); // N=1000/epoch, epoch_slots=100

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);

    // Slot 200: Alice unstakes fully
    env.set_clock(200);
    let (alice_col, alice_coin) = env.unstake_and_get_atas(&alice, 1_000_000);

    let alice_col_bal = env.read_token_balance(&alice_col);
    let alice_coin_bal = env.read_token_balance(&alice_coin);
    assert_eq!(alice_col_bal, 1_000_000, "Alice collateral fully returned");
    // epoch [100..200]: 1000 / 2 = ~500
    assert!(alice_coin_bal >= 499 && alice_coin_bal <= 500,
            "Alice COIN ~500, got {}", alice_coin_bal);

    // Slot 300: Bob unstakes — earned shared [100..200] + solo [200..300]
    env.set_clock(300);
    let (bob_col, bob_coin) = env.unstake_and_get_atas(&bob, 1_000_000);

    let bob_col_bal = env.read_token_balance(&bob_col);
    let bob_coin_bal = env.read_token_balance(&bob_coin);
    assert_eq!(bob_col_bal, 1_000_000, "Bob collateral fully returned");
    // [100..200] shared: ~500, [200..300] solo: ~1000 => ~1500
    assert!(bob_coin_bal >= 1498 && bob_coin_bal <= 1500,
            "Bob COIN ~1500, got {}", bob_coin_bal);
}

#[test]
fn test_three_users_staggered_entry_and_exit() {
    // Alice stakes at 100, Bob at 150, Carol at 200.
    // Alice unstakes at 250, Bob at 300, Carol at 350.
    // N=1200/epoch, epoch_slots=100.
    // Rate = 12 COIN/slot when divided among stakers.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1200, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    let carol = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&carol.pubkey(), 10_000_000_000).unwrap();

    // All stake equal amounts at different times
    // Slot 100: Alice stakes 1M (alone)
    env.stake(&alice, 1_000_000);

    // Slot 150: Bob stakes 1M (now Alice+Bob)
    env.set_clock(150);
    env.stake(&bob, 1_000_000);

    // Slot 200: Carol stakes 1M (now all three)
    env.set_clock(200);
    env.stake(&carol, 1_000_000);

    // Slot 250: Alice unstakes
    env.set_clock(250);
    let (alice_col, alice_coin) = env.unstake_and_get_atas(&alice, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 1_000_000);

    // Alice: [100..150] solo 50 slots = 600, [150..200] 1/2 50 slots = 300,
    //        [200..250] 1/3 50 slots = 200 => total 1100
    let alice_coin_bal = env.read_token_balance(&alice_coin);
    assert!(alice_coin_bal >= 1098 && alice_coin_bal <= 1100,
            "Alice COIN ~1100, got {}", alice_coin_bal);

    // Slot 300: Bob unstakes (Bob+Carol for [250..300])
    env.set_clock(300);
    let (bob_col, bob_coin) = env.unstake_and_get_atas(&bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);

    // Bob: [150..200] 1/2 = 300, [200..250] 1/3 = 200, [250..300] 1/2 = 300 => 800
    let bob_coin_bal = env.read_token_balance(&bob_coin);
    assert!(bob_coin_bal >= 798 && bob_coin_bal <= 800,
            "Bob COIN ~800, got {}", bob_coin_bal);

    // Slot 350: Carol unstakes (solo for [300..350])
    env.set_clock(350);
    let (carol_col, carol_coin) = env.unstake_and_get_atas(&carol, 1_000_000);
    assert_eq!(env.read_token_balance(&carol_col), 1_000_000);

    // Carol: [200..250] 1/3 = 200, [250..300] 1/2 = 300, [300..350] solo = 600 => 1100
    let carol_coin_bal = env.read_token_balance(&carol_coin);
    assert!(carol_coin_bal >= 1098 && carol_coin_bal <= 1100,
            "Carol COIN ~1100, got {}", carol_coin_bal);
}

#[test]
fn test_partial_unstake_then_full_unstake_different_users() {
    // Alice stakes 2M, Bob stakes 1M. Alice partial-unstakes 1M at slot 200,
    // then fully unstakes remaining 1M at slot 300. Bob fully unstakes at 300.
    // Verify collateral and rewards at each step.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(900, 100); // 9 COIN/slot

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 2_000_000); // 2/3 of pool
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);   // 1/3 of pool

    // Slot 200: Alice partial-unstakes 1M
    env.set_clock(200);
    let (alice_col_1, alice_coin_1) = env.unstake_and_get_atas(&alice, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col_1), 1_000_000);

    // [100..200]: Alice 2/3 of 900 = 600
    let alice_coin_1_bal = env.read_token_balance(&alice_coin_1);
    assert!(alice_coin_1_bal >= 599 && alice_coin_1_bal <= 600,
            "Alice partial COIN ~600, got {}", alice_coin_1_bal);

    // Slot 300: Both withdraw fully (Alice has 1M left, Bob has 1M, pool = 2M)
    env.set_clock(300);
    let (alice_col_2, alice_coin_2) = env.unstake_and_get_atas(&alice, 1_000_000);
    let (bob_col, bob_coin) = env.unstake_and_get_atas(&bob, 1_000_000);

    assert_eq!(env.read_token_balance(&alice_col_2), 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);

    // [200..300]: pool=2M, each has 1M => each gets 900/2 = 450
    let alice_coin_2_bal = env.read_token_balance(&alice_coin_2);
    assert!(alice_coin_2_bal >= 449 && alice_coin_2_bal <= 450,
            "Alice remaining COIN ~450, got {}", alice_coin_2_bal);

    // Bob total: [100..200] 1/3 = 300, [200..300] 1/2 = 450 => 750
    let bob_coin_bal = env.read_token_balance(&bob_coin);
    assert!(bob_coin_bal >= 749 && bob_coin_bal <= 750,
            "Bob COIN ~750, got {}", bob_coin_bal);
}

#[test]
fn test_claim_then_unstake_no_double_rewards() {
    // Alice stakes, claims rewards at slot 200, then unstakes at slot 300.
    // Total COIN should equal what she'd get by just unstaking at 300.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 1_000_000);

    // Slot 200: Claim (no unstake)
    env.set_clock(200);
    let claim_ata = env.claim_stake_rewards(&alice);
    let claimed = env.read_token_balance(&claim_ata);
    // Solo for 1 epoch => ~1000
    assert!(claimed >= 999 && claimed <= 1000, "Claimed ~1000, got {}", claimed);

    // Slot 300: Unstake — should get rewards for [200..300] only, not double
    env.set_clock(300);
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&alice, 1_000_000);

    assert_eq!(env.read_token_balance(&col_ata), 1_000_000);
    let unstake_coin = env.read_token_balance(&coin_ata);
    // [200..300] solo => ~1000
    assert!(unstake_coin >= 999 && unstake_coin <= 1000,
            "Unstake COIN ~1000, got {}", unstake_coin);

    // Total: claimed + unstake_coin should be ~2000 (2 epochs solo)
    let total = claimed + unstake_coin;
    assert!(total >= 1998 && total <= 2000, "Total COIN ~2000, got {}", total);
}

#[test]
fn test_user_leaves_mid_epoch_collateral_conserved() {
    // Verify total collateral in vault equals sum of all staked positions.
    // Alice stakes 2M, Bob stakes 1M. Alice unstakes 500K. Vault should have 2.5M.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 2_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);

    // Check vault has 3M
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()],
        &env.rewards_id,
    );
    assert_eq!(env.read_token_balance(&stake_vault), 3_000_000);

    // Alice partial unstake 500K
    env.set_clock(200);
    let (alice_col, _) = env.unstake_and_get_atas(&alice, 500_000);
    assert_eq!(env.read_token_balance(&alice_col), 500_000);
    assert_eq!(env.read_token_balance(&stake_vault), 2_500_000);

    // Bob full unstake
    env.set_clock(300);
    let (bob_col, _) = env.unstake_and_get_atas(&bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);
    assert_eq!(env.read_token_balance(&stake_vault), 1_500_000);
}

// ============================================================================
// Tests: admin burn disables all admin instructions
// ============================================================================

fn try_percolator_admin_ix_2(
    env: &mut TestEnv,
    admin: &Keypair,
    data: Vec<u8>,
) -> Result<(), String> {
    let ix = Instruction {
        program_id: env.percolator_id,
        accounts: vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.slab, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&admin.pubkey()),
        &[admin],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

/// UpdateAuthority (tag 32) — takes 3 accounts: [current_signer, new_authority, slab]
fn try_update_admin(
    env: &mut TestEnv,
    admin: &Keypair,
    new_admin: &Pubkey,
) -> Result<(), String> {
    let ix = Instruction {
        program_id: env.percolator_id,
        accounts: vec![
            AccountMeta::new_readonly(admin.pubkey(), true),
            AccountMeta::new_readonly(*new_admin, false),
            AccountMeta::new(env.slab, false),
        ],
        data: encode_update_admin(new_admin),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&admin.pubkey()),
        &[admin],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

fn try_percolator_admin_ix_6(
    env: &mut TestEnv,
    admin: &Keypair,
    data: Vec<u8>,
) -> Result<(), String> {
    let dummy = Pubkey::new_unique();
    let ix = Instruction {
        program_id: env.percolator_id,
        accounts: vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(dummy, false),
            AccountMeta::new(env.vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(dummy, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&admin.pubkey()),
        &[admin],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

fn try_percolator_admin_ix_8(
    env: &mut TestEnv,
    admin: &Keypair,
    data: Vec<u8>,
) -> Result<(), String> {
    let dummy = Pubkey::new_unique();
    let ix = Instruction {
        program_id: env.percolator_id,
        accounts: vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(env.vault, false),
            AccountMeta::new(dummy, false),
            AccountMeta::new_readonly(dummy, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(dummy, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&admin.pubkey()),
        &[admin],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?}", e))
}

#[test]
fn test_admin_burn_disables_all_admin_instructions() {
    let mut env = TestEnv::new();

    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();
    // Verify admin works by updating admin to self (no-op)
    let result = try_update_admin(&mut env, &admin, &admin.pubkey());
    assert!(result.is_ok(), "Admin should work before burn: {:?}", result);

    // Now burn admin
    env.advance_blockhash();
    let result = try_update_admin(&mut env, &admin, &Pubkey::default());
    assert!(result.is_ok(), "UpdateAdmin to zero should succeed: {:?}", result);

    let anyone = Keypair::new();
    env.svm.airdrop(&anyone.pubkey(), 10_000_000_000).unwrap();

    // Admin instructions must fail after burn
    env.advance_blockhash();
    let r = try_update_admin(&mut env, &anyone, &anyone.pubkey());
    assert!(r.is_err(), "UpdateAdmin must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_close_slab());
    assert!(r.is_err(), "CloseSlab must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_update_config());
    assert!(r.is_err(), "UpdateConfig must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_oracle_authority(&anyone.pubkey()));
    assert!(r.is_err(), "SetOracleAuthority must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_resolve_market());
    assert!(r.is_err(), "ResolveMarket must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &anyone, encode_withdraw_insurance());
    assert!(r.is_err(), "WithdrawInsurance must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_8(&mut env, &anyone, encode_admin_force_close(0));
    assert!(r.is_err(), "AdminForceCloseAccount must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &anyone,
        encode_set_insurance_withdraw_policy(&anyone.pubkey(), 1_000_000, 5000, 100),
    );
    assert!(r.is_err(), "SetInsuranceWithdrawPolicy must fail after admin burn");

    env.advance_blockhash();
    let r = try_update_admin(&mut env, &admin, &admin.pubkey());
    assert!(r.is_err(), "Original admin must also fail after burn");
}

#[test]
fn test_admin_burn_is_irreversible() {
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let result = try_update_admin(&mut env, &admin, &Pubkey::default());
    assert!(result.is_ok());

    env.advance_blockhash();
    let r = try_update_admin(&mut env, &admin, &admin.pubkey());
    assert!(r.is_err(), "Cannot re-claim admin once burned");

    let new_admin = Keypair::new();
    env.svm.airdrop(&new_admin.pubkey(), 1_000_000_000).unwrap();
    env.advance_blockhash();
    let r = try_update_admin(&mut env, &new_admin, &new_admin.pubkey());
    assert!(r.is_err(), "No one can re-claim admin once burned");
}

// ============================================================================
// Tests: DAO cannot steal user funds
// ============================================================================

#[test]
fn test_dao_cannot_steal_via_admin_instructions() {
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let user = Keypair::new();
    let user_idx = env.init_user(&user);
    env.deposit(&user, user_idx, 100_000_000);

    let r = try_update_admin(&mut env, &admin, &Pubkey::default());
    assert!(r.is_ok(), "Admin burn should succeed");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_resolve_market());
    assert!(r.is_err(), "Cannot resolve market after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &admin, encode_withdraw_insurance());
    assert!(r.is_err(), "Cannot withdraw insurance after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_8(&mut env, &admin, encode_admin_force_close(user_idx));
    assert!(r.is_err(), "Cannot force close accounts after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_close_slab());
    assert!(r.is_err(), "Cannot close slab after admin burn");
}

#[test]
fn test_no_instruction_to_redirect_user_funds() {
    // Verify every attack path: attacker tries to use depositor's SP PDA
    // to unstake/claim. Verify vault balance and depositor position unchanged.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let depositor = Keypair::new();
    env.svm
        .airdrop(&depositor.pubkey(), 10_000_000_000)
        .unwrap();
    env.stake(&depositor, 1_000_000);
    let vault_before = env.vault_balance();

    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();

    env.set_clock(200);

    // Attack 1: attacker tries unstake using depositor's SP PDA
    let (mrc_pda, _) = Pubkey::find_program_address(
        &[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let (depositor_sp, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), depositor.pubkey().as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let attacker_col = env.create_ata(&col_mint, &attacker.pubkey(), 0);
    let attacker_coin = env.create_coin_ata(&attacker.pubkey(), 0);
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(attacker_col, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(depositor_sp, false), // depositor's SP!
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(attacker_coin, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_unstake(1_000_000),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&attacker.pubkey()), &[&attacker], env.svm.latest_blockhash());
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Attacker must not unstake depositor's position");

    // Verify vault unchanged — no partial execution
    assert_eq!(env.vault_balance(), vault_before, "Vault must be unchanged after failed attack");

    // Attack 2: attacker tries claim using depositor's SP PDA
    env.advance_blockhash();
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(depositor_sp, false), // depositor's SP!
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(attacker_coin, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_claim_stake_rewards(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&attacker.pubkey()), &[&attacker], env.svm.latest_blockhash());
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Attacker must not claim depositor's rewards");

    // Verify attacker received nothing
    assert_eq!(env.read_token_balance(&attacker_col), 0, "Attacker must get 0 collateral");
    assert_eq!(env.read_token_balance(&attacker_coin), 0, "Attacker must get 0 COIN");

    // Verify depositor's position is intact
    let sp_data = env.svm.get_account(&depositor_sp).unwrap();
    let amount = u64::from_le_bytes(sp_data.data[8..16].try_into().unwrap());
    assert_eq!(amount, 1_000_000, "Depositor's position must be intact");
}

#[test]
fn test_insurance_topup_permissionless_withdraw_restricted() {
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let user = Keypair::new();
    let user_idx = env.init_user(&user);
    env.deposit(&user, user_idx, 100_000_000);

    let donor = Keypair::new();
    env.svm.airdrop(&donor.pubkey(), 10_000_000_000).unwrap();
    let col_mint = env.collateral_mint;
    let donor_ata = env.create_ata(&col_mint, &donor.pubkey(), 10_000_000);
    let ix = Instruction {
        program_id: env.percolator_id,
        accounts: vec![
            AccountMeta::new(donor.pubkey(), true),
            AccountMeta::new(env.slab, false),
            AccountMeta::new(donor_ata, false),
            AccountMeta::new(env.vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_topup_insurance(1_000_000),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&donor.pubkey()),
        &[&donor],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .expect("insurance topup should succeed");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &admin, encode_withdraw_insurance());
    assert!(r.is_err(), "Cannot withdraw insurance before resolution");

    env.advance_blockhash();
    let r = try_update_admin(&mut env, &admin, &Pubkey::default());
    assert!(r.is_ok());

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_resolve_market());
    assert!(r.is_err(), "Cannot resolve after burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &admin, encode_withdraw_insurance());
    assert!(r.is_err(), "Cannot withdraw insurance after burn");
}

// ============================================================================
// Tests: full end-to-end insurance deposit flow
// ============================================================================

#[test]
fn test_e2e_deposit_earn_withdraw() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 500u64;
    let epoch_slots = 100u64;

    env.init_market_rewards(n, epoch_slots);

    // Set up depositor
    let depositor = Keypair::new();
    env.svm
        .airdrop(&depositor.pubkey(), 10_000_000_000)
        .unwrap();

    // Deposit collateral
    env.stake(&depositor, 2_000_000);

    // Advance 2 epochs
    env.set_clock(300);

    // Claim COIN rewards
    let coin_ata = env.create_coin_ata(&depositor.pubkey(), 0);
    env.claim_stake_rewards_to(&depositor, &coin_ata);
    let reward = env.read_token_balance(&coin_ata);
    // 500 * 2 = ~1000 (sole depositor for 2 epochs)
    assert!(reward >= 998 && reward <= 1000, "Depositor: ~1000, got {}", reward);

    // Withdraw collateral
    env.advance_blockhash();
    let (col_ata, _) = env.unstake_and_get_atas(&depositor, 2_000_000);
    let col_balance = env.read_token_balance(&col_ata);
    assert_eq!(col_balance, 2_000_000, "Should get all collateral back");
}

// ============================================================================
// Tests: unauthorized market cannot inflate shared COIN
// ============================================================================

#[test]
fn test_unauthorized_market_cannot_inflate_shared_coin() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let rogue_slab = Pubkey::new_unique();
    env.svm
        .set_account(
            rogue_slab,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; SLAB_LEN],
                owner: env.percolator_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", rogue_slab.as_ref()], &env.rewards_id);
    let (coin_cfg_pda, _) = Pubkey::find_program_address(
        &[b"coin_cfg", env.coin_mint.as_ref()],
        &env.rewards_id,
    );
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", rogue_slab.as_ref()],
        &env.rewards_id,
    );

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new_readonly(attacker.pubkey(), true),
            AccountMeta::new_readonly(rogue_slab, false),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(env.collateral_mint, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(u64::MAX, 100),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&attacker.pubkey()),
        &[&attacker],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Attacker cannot register market with shared COIN");
}

// ============================================================================
// Tests: non-signer rejection
// ============================================================================

#[test]
fn test_stake_non_signer_rejected() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (stake_vault, _) =
        Pubkey::find_program_address(&[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let col_mint = env.collateral_mint;
    let user_ata = env.create_ata(&col_mint, &user.pubkey(), 500);

    // Build instruction with user NOT as signer
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), false), // NOT a signer
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(sp_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_stake(500),
    };
    let payer = &env.payer;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Stake without signer must fail");
}

#[test]
fn test_unstake_non_signer_rejected() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 500);
    env.set_clock(300);

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (stake_vault, _) =
        Pubkey::find_program_address(&[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let col_mint = env.collateral_mint;
    let user_ata = env.create_ata(&col_mint, &user.pubkey(), 0);
    let coin_ata = env.create_coin_ata(&user.pubkey(), 0);

    // Build instruction with user NOT as signer
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), false), // NOT a signer
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(sp_pda, false),
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(coin_ata, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_unstake(500),
    };
    let payer = &env.payer;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Unstake without signer must fail");
}

#[test]
fn test_claim_stake_rewards_non_signer_rejected() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 500);
    env.set_clock(200);

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let coin_ata = env.create_coin_ata(&user.pubkey(), 0);

    // Build instruction with user NOT as signer
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), false), // NOT a signer
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(sp_pda, false),
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(coin_ata, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_claim_stake_rewards(),
    };
    let payer = &env.payer;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Claim without signer must fail");
}


// ============================================================================
// Tests: wrong MRC / slab mismatch
// ============================================================================

#[test]
fn test_stake_wrong_slab_mismatch_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // Use a different slab key that doesn't match MRC
    let wrong_slab = Pubkey::new_unique();
    env.svm
        .set_account(
            wrong_slab,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; SLAB_LEN],
                owner: env.percolator_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (stake_vault, _) =
        Pubkey::find_program_address(&[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    // SP derived from wrong slab — will fail PDA check too
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let col_mint = env.collateral_mint;
    let user_ata = env.create_ata(&col_mint, &user.pubkey(), 500);

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(wrong_slab, false), // wrong slab
            AccountMeta::new(user_ata, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(sp_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_stake(500),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&user.pubkey()),
        &[&user],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Stake with wrong slab must fail");
}

// ============================================================================
// Tests: unstake wrong stake_vault PDA
// ============================================================================

#[test]
fn test_unstake_wrong_stake_vault_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 500);
    env.set_clock(300);

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let col_mint = env.collateral_mint;
    let user_ata = env.create_ata(&col_mint, &user.pubkey(), 0);
    let coin_ata = env.create_coin_ata(&user.pubkey(), 0);

    // Create a fake vault that is NOT the correct PDA
    let fake_vault = Pubkey::new_unique();
    let (mrc_key, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    env.svm
        .set_account(
            fake_vault,
            Account {
                lamports: 1_000_000,
                data: make_token_account_data(&col_mint, &mrc_key, 500),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(fake_vault, false), // wrong vault
            AccountMeta::new(sp_pda, false),
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(coin_ata, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_unstake(500),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&user.pubkey()),
        &[&user],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Unstake with wrong vault PDA must fail");
}

// ============================================================================
// Tests: init_market_rewards with uninitialized slab (market_start_slot=0)
// ============================================================================

#[test]
fn test_init_market_rewards_uninitialized_slab_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    // Create a raw slab that was never initialized via InitMarket
    // (market_start_slot will be 0)
    let raw_slab = Pubkey::new_unique();
    env.svm
        .set_account(
            raw_slab,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; SLAB_LEN],
                owner: env.percolator_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_market_rewards_for_slab(&raw_slab, 1000, 100);
    assert!(
        result.is_err(),
        "init_market_rewards must reject slab with market_start_slot=0"
    );
}

// ============================================================================
// Tests: two markets sharing one COIN work independently
// ============================================================================

#[test]
fn test_two_markets_share_one_coin() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    // Create a second percolator market with its own slab
    let slab2 = Pubkey::new_unique();
    let vault2 = Pubkey::new_unique();
    let (vault2_pda, _) =
        Pubkey::find_program_address(&[b"vault", slab2.as_ref()], &env.percolator_id);
    env.svm
        .set_account(
            slab2,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; SLAB_LEN],
                owner: env.percolator_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.svm
        .set_account(
            vault2,
            Account {
                lamports: 1_000_000,
                data: make_token_account_data(&env.collateral_mint, &vault2_pda, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // Init market 2 via percolator
    let dummy_ata2 = Pubkey::new_unique();
    env.svm
        .set_account(
            dummy_ata2,
            Account {
                lamports: 1_000_000,
                data: vec![0u8; TokenAccount::LEN],
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let ix = Instruction {
        program_id: env.percolator_id,
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new(slab2, false),
            AccountMeta::new_readonly(env.collateral_mint, false),
            AccountMeta::new(vault2, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(dummy_ata2, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market(
            &env.payer.pubkey(),
            &env.collateral_mint,
            &TEST_FEED_ID,
            0,
        ),
    };
    let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix, ix],
        Some(&env.payer.pubkey()),
        &[&env.payer],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("init_market2 failed");

    // Init rewards for market 2 (different N)
    let (mrc_pda2, _) =
        Pubkey::find_program_address(&[b"mrc", slab2.as_ref()], &env.rewards_id);
    let (stake_vault2, _) =
        Pubkey::find_program_address(&[b"stake_vault", slab2.as_ref()], &env.rewards_id);
    env.register_insurance_operator_for_slab(&slab2);
    env.burn_market_admin_for_slab(&slab2);
    env.try_init_market_rewards_for_slab(&slab2, 2000, 100)
        .expect("init_market_rewards2 failed");

    // Stake on market 1
    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.stake(&alice, 500); // market 1

    // Stake on market 2 (manually since helpers use env.slab)
    let bob = Keypair::new();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    let col_mint = env.collateral_mint;
    let bob_ata = env.create_ata(&col_mint, &bob.pubkey(), 500);
    let (sp_pda_bob, _) = Pubkey::find_program_address(
        &[b"sp", slab2.as_ref(), bob.pubkey().as_ref()],
        &env.rewards_id,
    );

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(bob.pubkey(), true),
            AccountMeta::new(mrc_pda2, false),
            AccountMeta::new_readonly(slab2, false),
            AccountMeta::new(bob_ata, false),
            AccountMeta::new(stake_vault2, false),
            AccountMeta::new(sp_pda_bob, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_stake(500),
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&bob.pubkey()),
        &[&bob],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("bob stake market2 failed");

    // Advance 100 slots (1 epoch)
    env.set_clock(200);

    // Claim from market 1 (Alice): should get ~1000 COIN
    let alice_coin = env.claim_stake_rewards(&alice);
    let alice_bal = env.read_token_balance(&alice_coin);
    assert!(
        alice_bal >= 999 && alice_bal <= 1001,
        "Alice (market1, N=1000) should get ~1000 COIN, got {}",
        alice_bal
    );

    // Claim from market 2 (Bob): should get ~2000 COIN
    let bob_coin = env.create_coin_ata(&bob.pubkey(), 0);
    let (sp_pda_bob2, _) = Pubkey::find_program_address(
        &[b"sp", slab2.as_ref(), bob.pubkey().as_ref()],
        &env.rewards_id,
    );
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(bob.pubkey(), true),
            AccountMeta::new(mrc_pda2, false),
            AccountMeta::new_readonly(slab2, false),
            AccountMeta::new(sp_pda_bob2, false),
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(bob_coin, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_claim_stake_rewards(),
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&bob.pubkey()),
        &[&bob],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("bob claim market2 failed");

    let bob_bal = env.read_token_balance(&bob_coin);
    assert!(
        bob_bal >= 1999 && bob_bal <= 2001,
        "Bob (market2, N=2000) should get ~2000 COIN, got {}",
        bob_bal
    );
}

// ============================================================================
// Tests: N=0 (no rewards emitted)
// ============================================================================

#[test]
fn test_n_zero_no_rewards_emitted() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(0, 100); // N=0: no staking rewards

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 500);
    env.set_clock(200); // 1 full epoch

    let coin_ata = env.claim_stake_rewards(&user);
    let bal = env.read_token_balance(&coin_ata);
    assert_eq!(bal, 0, "N=0 should emit zero COIN rewards, got {}", bal);

    // User can still unstake their collateral
    env.set_clock(300);
    let (col_ata, _) = env.unstake_and_get_atas(&user, 500);
    let col_bal = env.read_token_balance(&col_ata);
    assert_eq!(col_bal, 500, "Collateral must be returned even with N=0");
}

// ============================================================================
// Tests: unstake must verify SP PDA belongs to the signer
// ============================================================================

#[test]
fn test_unstake_wrong_user_sp_rejected() {
    // Alice stakes. Bob (attacker) tries to unstake Alice's position to his own ATAs.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.stake(&alice, 500);
    env.set_clock(300);

    let bob = Keypair::new();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Bob builds an unstake tx using Alice's SP PDA but his own ATAs
    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (stake_vault, _) =
        Pubkey::find_program_address(&[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    // Alice's stake position PDA
    let (alice_sp, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), alice.pubkey().as_ref()],
        &env.rewards_id,
    );

    let col_mint = env.collateral_mint;
    let bob_col_ata = env.create_ata(&col_mint, &bob.pubkey(), 0);
    let bob_coin_ata = env.create_coin_ata(&bob.pubkey(), 0);

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(bob.pubkey(), true),             // Bob is the signer
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(bob_col_ata, false),             // Bob's collateral ATA
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(alice_sp, false),                // Alice's SP PDA!
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(bob_coin_ata, false),            // Bob's COIN ATA
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_unstake(500),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&bob.pubkey()),
        &[&bob],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(
        result.is_err(),
        "Attacker must not be able to unstake another user's position"
    );
}

// ============================================================================
// Tests: adversarial insurance withdrawal scenarios
// ============================================================================

#[test]
fn test_immediate_withdraw_returns_deposit_zero_rewards() {
    // Adversarial: deposit and immediately withdraw in same slot.
    // Should get full deposit back but zero COIN (no time elapsed).
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // Deposit and immediately withdraw (same slot 100)
    env.stake(&user, 1_000_000);
    env.advance_blockhash();
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);

    assert_eq!(env.read_token_balance(&col_ata), 1_000_000,
               "Full deposit must be returned on immediate withdraw");
    assert_eq!(env.read_token_balance(&coin_ata), 0,
               "Zero COIN for zero elapsed time");
}

#[test]
fn test_early_withdraw_proportional_rewards() {
    // User deposits for only 1/4 of an epoch, gets proportional COIN.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100); // 10 COIN/slot for sole depositor

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // Deposit at slot 100
    env.stake(&user, 1_000_000);

    // Withdraw at slot 125 (25 slots = 1/4 epoch)
    env.set_clock(125);
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);

    assert_eq!(env.read_token_balance(&col_ata), 1_000_000,
               "Full deposit returned");
    // 1000 * 25/100 = 250 COIN
    let coin_bal = env.read_token_balance(&coin_ata);
    assert!(coin_bal >= 249 && coin_bal <= 250,
            "25 slots should earn ~250 COIN, got {}", coin_bal);
}

#[test]
fn test_late_withdraw_accumulates_all_rewards() {
    // User deposits and stays for 10 epochs. Gets all accumulated COIN.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100); // 1000 COIN/epoch

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // Deposit at slot 100
    env.stake(&user, 1_000_000);

    // Stay for 10 epochs (1000 slots)
    env.set_clock(1100);
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);

    assert_eq!(env.read_token_balance(&col_ata), 1_000_000,
               "Full deposit returned after 10 epochs");
    let coin_bal = env.read_token_balance(&coin_ata);
    // 1000 * 10 = 10000 COIN
    assert!(coin_bal >= 9998 && coin_bal <= 10000,
            "10 epochs solo should earn ~10000 COIN, got {}", coin_bal);
}

#[test]
fn test_withdraw_redeposit_withdraw_cycle() {
    // Two users cycle through deposit/withdraw at different times.
    // Each earns independently. No double-counting.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Alice: deposit at 100, withdraw at 200 (1 epoch solo)
    env.stake(&alice, 1_000_000);
    env.set_clock(200);
    let (col_a, coin_a) = env.unstake_and_get_atas(&alice, 1_000_000);
    assert_eq!(env.read_token_balance(&col_a), 1_000_000);
    let alice_coin = env.read_token_balance(&coin_a);
    assert!(alice_coin >= 999 && alice_coin <= 1000, "Alice: ~1000, got {}", alice_coin);

    // Bob: deposit at 300, withdraw at 500 (2 epochs solo)
    env.set_clock(300);
    env.stake(&bob, 1_000_000);
    env.set_clock(500);
    let (col_b, coin_b) = env.unstake_and_get_atas(&bob, 1_000_000);
    assert_eq!(env.read_token_balance(&col_b), 1_000_000);
    let bob_coin = env.read_token_balance(&coin_b);
    // 1000 * 2 = 2000 COIN for 2 epochs
    assert!(bob_coin >= 1998 && bob_coin <= 2000, "Bob: ~2000, got {}", bob_coin);

    // Each cycle independent — total matches expected
    let total = alice_coin + bob_coin;
    assert!(total >= 2997 && total <= 3000, "Total: ~3000, got {}", total);
}

#[test]
fn test_adversarial_flash_deposit_no_extra_rewards() {
    // Alice is the sole depositor. Bob "flash deposits" for 0 elapsed slots
    // to try and dilute Alice's rewards or extract free COIN.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Alice deposits at slot 100
    env.stake(&alice, 1_000_000);

    // At slot 150, Bob flash-deposits and immediately withdraws
    env.set_clock(150);
    env.stake(&bob, 1_000_000);
    env.advance_blockhash();
    let (bob_col, bob_coin) = env.unstake_and_get_atas(&bob, 1_000_000);

    // Bob gets deposit back but 0 COIN (deposited and withdrew in same slot)
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);
    assert_eq!(env.read_token_balance(&bob_coin), 0,
               "Flash deposit must earn zero COIN");

    // Alice continues earning solo for full 2 epochs
    env.set_clock(300);
    let alice_coin = env.claim_stake_rewards(&alice);
    let alice_bal = env.read_token_balance(&alice_coin);
    // Slots [100..150]: solo = 500, [150..300]: solo = 1500. Total = 2000
    // Bob's flash deposit at slot 150 settled Alice's pending but didn't dilute
    // because bob deposited and withdrew at same slot (total_staked returned to 1M)
    assert!(alice_bal >= 1998 && alice_bal <= 2000,
            "Alice should get ~2000 COIN unaffected by flash deposit, got {}", alice_bal);
}

#[test]
fn test_multi_user_early_late_exit_reward_conservation() {
    // Three depositors with different holding periods.
    // Total COIN emitted must equal n_per_epoch * elapsed_epochs.
    // Tests that early/late exits don't create or destroy rewards.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1200, 100); // 12 COIN/slot

    let alice = Keypair::new();
    let bob = Keypair::new();
    let carol = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&carol.pubkey(), 10_000_000_000).unwrap();

    // All deposit equal amounts at the same time (slot 100)
    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);
    env.advance_blockhash();
    env.stake(&carol, 1_000_000);

    // Alice withdraws EARLY at slot 150 (50 slots, half epoch)
    // All three shared equally: each earns 1200 * 50 / (100 * 3) = 200
    env.set_clock(150);
    let (alice_col, alice_coin) = env.unstake_and_get_atas(&alice, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 1_000_000);
    let alice_reward = env.read_token_balance(&alice_coin);
    assert!(alice_reward >= 199 && alice_reward <= 200,
            "Alice (early exit): ~200, got {}", alice_reward);

    // Bob withdraws at slot 200 (1 epoch total)
    // [100..150]: 1/3 of 1200*50/100 = 200
    // [150..200]: 1/2 of 1200*50/100 = 300
    // Total Bob: ~500
    env.set_clock(200);
    let (bob_col, bob_coin) = env.unstake_and_get_atas(&bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);
    let bob_reward = env.read_token_balance(&bob_coin);
    assert!(bob_reward >= 499 && bob_reward <= 500,
            "Bob (mid exit): ~500, got {}", bob_reward);

    // Carol withdraws LATE at slot 400 (3 epochs total)
    // [100..150]: 1/3 = 200
    // [150..200]: 1/2 = 300
    // [200..400]: solo 1200*200/100 = 2400
    // Total Carol: ~2900
    env.set_clock(400);
    let (carol_col, carol_coin) = env.unstake_and_get_atas(&carol, 1_000_000);
    assert_eq!(env.read_token_balance(&carol_col), 1_000_000);
    let carol_reward = env.read_token_balance(&carol_coin);
    assert!(carol_reward >= 2898 && carol_reward <= 2900,
            "Carol (late exit): ~2900, got {}", carol_reward);

    // Conservation check: total COIN emitted should match expected.
    // Slots with depositors: [100..400] = 300 slots
    // Total expected = 1200 * 300 / 100 = 3600
    let total = alice_reward + bob_reward + carol_reward;
    assert!(total >= 3597 && total <= 3600,
            "Total COIN must be conserved: expected ~3600, got {}", total);
}

#[test]
fn test_adversarial_withdraw_during_zero_total_staked() {
    // After all depositors withdraw, accumulator should not break.
    // New depositor entering after a gap earns only for their active period.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Alice deposits at 100, withdraws at 200
    env.stake(&alice, 1_000_000);
    env.set_clock(200);
    let (_, alice_coin) = env.unstake_and_get_atas(&alice, 1_000_000);
    let alice_reward = env.read_token_balance(&alice_coin);
    assert!(alice_reward >= 999 && alice_reward <= 1000);

    // Gap: no depositors from 200 to 500 (rewards are not emitted to anyone)
    env.set_clock(500);

    // Bob deposits at 500, withdraws at 600
    env.stake(&bob, 1_000_000);
    env.set_clock(600);
    let (bob_col, bob_coin) = env.unstake_and_get_atas(&bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);
    let bob_reward = env.read_token_balance(&bob_coin);
    // Bob earns only for [500..600] = 1000 COIN, NOT for the gap period
    assert!(bob_reward >= 999 && bob_reward <= 1000,
            "Bob must earn only for active period, got {}", bob_reward);
}

// ============================================================================
// Tests: draw_insurance — profits only, depositor capital protected
// ============================================================================

#[test]
fn test_draw_depositor_capital_rejected() {
    // DAO CANNOT draw depositor capital. vault_balance == total_staked means 0 profit.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);
    // vault = 1M, total_staked = 1M, profit = 0

    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    let result = env.try_draw_insurance(1, &dao_dest);
    assert!(result.is_err(), "DAO must not draw depositor capital (profit = 0)");
    assert_eq!(env.vault_balance(), 1_000_000, "Vault untouched");
}

#[test]
fn test_draw_only_profits() {
    // DAO can draw profits (vault_balance - total_staked) but not more.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);
    // vault = 1M, total_staked = 1M

    // Inject 500K "profit" by sending tokens directly to vault
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let donor_ata = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 500_000);
    let xfer_ix = spl_token::instruction::transfer(
        &spl_token::ID, &donor_ata, &stake_vault,
        &env.dao_authority.pubkey(), &[], 500_000,
    ).unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[xfer_ix], Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority], env.svm.latest_blockhash());
    env.svm.send_transaction(tx).expect("inject profit");
    assert_eq!(env.vault_balance(), 1_500_000); // 1M deposit + 500K profit

    // DAO draws exactly the profit (500K)
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.draw_insurance(500_000, &dao_dest);
    assert_eq!(env.vault_balance(), 1_000_000, "Only profit drawn");
    assert_eq!(env.read_token_balance(&dao_dest), 500_000);

    // Cannot draw more — would eat into depositor capital
    env.advance_blockhash();
    let result = env.try_draw_insurance(1, &dao_dest);
    assert!(result.is_err(), "Cannot draw below total_staked");

    // User still gets full deposit back
    env.set_clock(200);
    let (col_ata, _) = env.unstake_and_get_atas(&user, 1_000_000);
    assert_eq!(env.read_token_balance(&col_ata), 1_000_000, "Full deposit returned");
}

#[test]
fn test_draw_all_remaining_after_depositors_withdraw() {
    // After all depositors withdraw (total_staked == 0), DAO can draw everything.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    // Inject 300K profit
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let donor_ata = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 300_000);
    let xfer_ix = spl_token::instruction::transfer(
        &spl_token::ID, &donor_ata, &stake_vault,
        &env.dao_authority.pubkey(), &[], 300_000,
    ).unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[xfer_ix], Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority], env.svm.latest_blockhash());
    env.svm.send_transaction(tx).expect("inject profit");

    // User withdraws — gets full deposit
    env.set_clock(200);
    let (col_ata, _) = env.unstake_and_get_atas(&user, 1_000_000);
    assert_eq!(env.read_token_balance(&col_ata), 1_000_000);
    // vault = 300K (profit remains), total_staked = 0
    assert_eq!(env.vault_balance(), 300_000);

    // DAO draws remaining profit
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.advance_blockhash();
    env.draw_insurance(300_000, &dao_dest);
    assert_eq!(env.vault_balance(), 0);
    assert_eq!(env.read_token_balance(&dao_dest), 300_000);
}

#[test]
fn test_depositors_always_get_full_deposit_back() {
    // Even after DAO draws all profits, depositors get 100%.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 2_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);
    // vault = 3M, total_staked = 3M

    // Inject 1M profit
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let donor_ata = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 1_000_000);
    let xfer_ix = spl_token::instruction::transfer(
        &spl_token::ID, &donor_ata, &stake_vault,
        &env.dao_authority.pubkey(), &[], 1_000_000,
    ).unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[xfer_ix], Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority], env.svm.latest_blockhash());
    env.svm.send_transaction(tx).expect("inject profit");
    // vault = 4M, total_staked = 3M, profit = 1M

    // DAO draws all profit
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.draw_insurance(1_000_000, &dao_dest);
    assert_eq!(env.vault_balance(), 3_000_000);

    // Both get full deposits back
    env.set_clock(200);
    let (alice_col, _) = env.unstake_and_get_atas(&alice, 2_000_000);
    let (bob_col, _) = env.unstake_and_get_atas(&bob, 1_000_000);

    assert_eq!(env.read_token_balance(&alice_col), 2_000_000, "Alice: full 2M");
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000, "Bob: full 1M");
    assert_eq!(env.vault_balance(), 0);
}

#[test]
fn test_draw_insurance_non_governance_rejected() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();
    let col_mint = env.collateral_mint;
    let attacker_dest = env.create_ata(&col_mint, &attacker.pubkey(), 0);

    let result = env.try_draw_insurance_direct(&attacker, 1, &attacker_dest);
    assert!(result.is_err(), "Non-governance must be rejected for draw_insurance");
    assert_eq!(env.vault_balance(), 1_000_000, "Vault must be untouched");
}

#[test]
fn test_draw_zero_amount_rejected() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    let result = env.try_draw_insurance(0, &dao_dest);
    assert!(result.is_err(), "Zero draw must be rejected");
}

// ============================================================================
// Tests: adversarial attacks
// ============================================================================

#[test]
fn test_adversarial_direct_vault_transfer_no_steal() {
    // Attacker sends tokens directly to vault. They can't get them back.
    // Depositors aren't harmed. DAO can draw the excess as profit.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    // Attacker sends 500K directly to vault
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();
    let attacker_ata = env.create_ata(&col_mint, &attacker.pubkey(), 500_000);
    let xfer_ix = spl_token::instruction::transfer(
        &spl_token::ID, &attacker_ata, &stake_vault,
        &attacker.pubkey(), &[], 500_000,
    ).unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[xfer_ix], Some(&attacker.pubkey()),
        &[&attacker], env.svm.latest_blockhash());
    env.svm.send_transaction(tx).expect("direct transfer");
    assert_eq!(env.vault_balance(), 1_500_000);

    // User withdraws — gets exactly their deposit (not the attacker's tokens)
    env.set_clock(200);
    let (col_ata, _) = env.unstake_and_get_atas(&user, 1_000_000);
    assert_eq!(env.read_token_balance(&col_ata), 1_000_000, "User gets exact deposit");

    // Attacker's tokens stuck as profit — DAO can draw them
    assert_eq!(env.vault_balance(), 500_000);
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.advance_blockhash();
    env.draw_insurance(500_000, &dao_dest);
    assert_eq!(env.vault_balance(), 0);
}

#[test]
fn test_adversarial_1_token_dilution_negligible() {
    // Attacker deposits 1 token to dilute large depositor's rewards.
    // Impact should be negligible.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let whale = Keypair::new();
    let attacker = Keypair::new();
    env.svm.airdrop(&whale.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    env.stake(&whale, 1_000_000);
    env.advance_blockhash();
    env.stake(&attacker, 1); // 1 token

    env.set_clock(200);
    let whale_coin = env.claim_stake_rewards(&whale);
    let whale_bal = env.read_token_balance(&whale_coin);
    // Whale has 1M / 1M+1 ≈ 99.9999% of pool. Should get ~999 COIN.
    assert!(whale_bal >= 998, "1-token dilution must be negligible, got {}", whale_bal);
}

#[test]
fn test_adversarial_withdraw_1_repeatedly_no_rounding_exploit() {
    // Withdraw 1 token at a time. Total must not exceed proportional share.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 100);

    env.set_clock(200);
    let mut total_withdrawn = 0u64;
    for _ in 0..100 {
        let (col_ata, _) = env.unstake_and_get_atas(&user, 1);
        total_withdrawn += env.read_token_balance(&col_ata);
        env.advance_blockhash();
    }
    // Must get back exactly 100 (no rounding exploitation)
    assert_eq!(total_withdrawn, 100, "Repeated 1-token withdrawals must total exact deposit");
    assert_eq!(env.vault_balance(), 0);
}

#[test]
fn test_adversarial_same_slot_triple_op() {
    // stake + claim + unstake in same slot. No extra rewards.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // All in slot 100
    env.stake(&user, 1_000_000);
    env.advance_blockhash();
    let claim_ata = env.claim_stake_rewards(&user);
    env.advance_blockhash();
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);

    assert_eq!(env.read_token_balance(&claim_ata), 0, "Claim in same slot = 0");
    assert_eq!(env.read_token_balance(&col_ata), 1_000_000, "Full deposit back");
    assert_eq!(env.read_token_balance(&coin_ata), 0, "Unstake in same slot = 0 COIN");
}

#[test]
fn test_adversarial_claim_then_unstake_no_double_rewards() {
    // Claim rewards, then unstake same epoch. Total COIN should equal single unstake.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    env.set_clock(200);
    let claim_ata = env.claim_stake_rewards(&user);
    let claimed = env.read_token_balance(&claim_ata);

    env.set_clock(300);
    let (_, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);
    let unstaked_coin = env.read_token_balance(&coin_ata);

    // Total = claimed + unstaked_coin should be ~2000 (2 epochs solo)
    let total = claimed + unstaked_coin;
    assert!(total >= 1998 && total <= 2000, "Total ~2000, no double count, got {}", total);
}

#[test]
fn test_adversarial_fake_mrc_rejected() {
    // Attacker creates an account with MRC discriminator and tries to stake.
    // Must fail because the account's key won't match the expected PDA.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    let fake_mrc = Pubkey::new_unique();
    let mut fake_data = vec![0u8; 160];
    fake_data[..8].copy_from_slice(b"MRC_V003");
    // Put slab key in the right spot
    fake_data[8..40].copy_from_slice(env.slab.as_ref());
    env.svm.set_account(fake_mrc, Account {
        lamports: 1_000_000,
        data: fake_data,
        owner: env.rewards_id,
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), attacker.pubkey().as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let attacker_ata = env.create_ata(&col_mint, &attacker.pubkey(), 500);

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new(fake_mrc, false), // fake MRC
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(attacker_ata, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(sp_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_stake(500),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&attacker.pubkey()), &[&attacker], env.svm.latest_blockhash());
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Fake MRC must be rejected (PDA mismatch)");
}

#[test]
fn test_adversarial_steal_via_wrong_sp_pda() {
    // Alice deposits. Bob tries to unstake Alice's SP by passing it to unstake.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.stake(&alice, 1_000_000);

    env.set_clock(200);
    let (mrc_pda, _) = Pubkey::find_program_address(
        &[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let (alice_sp, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), alice.pubkey().as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let bob_col = env.create_ata(&col_mint, &bob.pubkey(), 0);
    let bob_coin = env.create_coin_ata(&bob.pubkey(), 0);

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(bob.pubkey(), true),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(bob_col, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new(alice_sp, false), // Alice's SP!
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(bob_coin, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_unstake(1_000_000),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&bob.pubkey()), &[&bob], env.svm.latest_blockhash());
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Must not steal via wrong SP PDA");
    assert_eq!(env.vault_balance(), 1_000_000, "Alice's deposit safe");
}

#[test]
fn test_depositor_capital_protected_after_profit_draw() {
    // Multiple depositors + profit injection + profit draw.
    // All depositors must get 100% back after DAO draws only profits.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 2_000_000);
    // vault = 3M, total_staked = 3M

    // Inject 600K profit
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let donor_ata = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 600_000);
    let xfer_ix = spl_token::instruction::transfer(
        &spl_token::ID, &donor_ata, &stake_vault,
        &env.dao_authority.pubkey(), &[], 600_000,
    ).unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[xfer_ix], Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority], env.svm.latest_blockhash());
    env.svm.send_transaction(tx).expect("inject profit");
    // vault = 3.6M, total_staked = 3M, profit = 600K

    // DAO draws profit
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.draw_insurance(600_000, &dao_dest);
    assert_eq!(env.vault_balance(), 3_000_000);

    // Both get full deposits
    env.set_clock(200);
    let (alice_col, _) = env.unstake_and_get_atas(&alice, 1_000_000);
    let (bob_col, _) = env.unstake_and_get_atas(&bob, 2_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 2_000_000);
    assert_eq!(env.vault_balance(), 0);
}

#[test]
fn test_withdrawal_always_works_no_governance_block() {
    // Even with zero profit, depositor can always withdraw.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    // No profit, no draw — just verify withdrawal works
    env.set_clock(200);
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);
    assert_eq!(env.read_token_balance(&col_ata), 1_000_000);
    let coin_bal = env.read_token_balance(&coin_ata);
    assert!(coin_bal >= 999 && coin_bal <= 1000);
}

#[test]
fn test_claim_rewards_always_works() {
    // claim_stake_rewards always succeeds regardless of vault state.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    env.set_clock(200);
    let coin_ata = env.claim_stake_rewards(&user);
    let coin_bal = env.read_token_balance(&coin_ata);
    assert!(coin_bal >= 999 && coin_bal <= 1000);
}

// ============================================================================
// Tests: defense-in-depth — owner checks
// ============================================================================

#[test]
fn test_init_coin_config_non_spl_mint_rejected() {
    // A crafted account with Mint-shaped data but wrong owner must be rejected.
    let mut env = TestEnv::new();

    let fake_mint = Pubkey::new_unique();
    let (fake_mint_auth, _) = Pubkey::find_program_address(
        &[b"coin_mint_authority", fake_mint.as_ref()],
        &env.rewards_id,
    );
    // Craft a fake mint: correct format but owned by a random program
    let fake_owner = Pubkey::new_unique();
    env.svm.set_account(fake_mint, Account {
        lamports: 1_000_000,
        data: make_mint_data_with_authority(&fake_mint_auth),
        owner: fake_owner, // NOT spl_token::ID
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    let result = env.try_init_coin_config_with_mint(&fake_mint);
    assert!(result.is_err(), "Non-SPL-owned mint must be rejected");
}

// ============================================================================
// Tests: exercise dead-code paths an adversarial dev could sabotage
// ============================================================================

#[test]
fn test_proportional_withdrawal_defense_in_depth() {
    // The proportional withdrawal path (vault_balance < total_staked) can't be
    // reached via normal program operations because draw_insurance prevents it.
    // An adversarial dev could hide a bug here knowing no test would reach it.
    // We test it by directly manipulating the vault balance in LiteSVM.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);
    // vault = 2M, total_staked = 2M

    // Directly reduce vault balance to 1M (bypassing draw_insurance constraint)
    // to exercise the proportional withdrawal code path.
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let vault_acct = env.svm.get_account(&stake_vault).unwrap();
    let vault_token = TokenAccount::unpack(&vault_acct.data).unwrap();
    let (mrc_pda, _) = Pubkey::find_program_address(
        &[b"mrc", env.slab.as_ref()], &env.rewards_id);
    // Rebuild the token account data with 1M instead of 2M
    let reduced_data = make_token_account_data(&env.collateral_mint, &mrc_pda, 1_000_000);
    env.svm.set_account(stake_vault, Account {
        lamports: vault_acct.lamports,
        data: reduced_data,
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();
    assert_eq!(env.vault_balance(), 1_000_000);
    // Now: vault = 1M, total_staked = 2M → ratio = 50%

    env.set_clock(200);

    // Alice withdraws: should get 1M * 1M/2M = 500K (proportional)
    let (alice_col, _) = env.unstake_and_get_atas(&alice, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 500_000,
               "Proportional withdrawal must give 50% when vault is 50% funded");

    // After Alice: vault = 500K, total_staked = 1M
    // Bob withdraws: should get 1M * 500K/1M = 500K
    let (bob_col, _) = env.unstake_and_get_atas(&bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 500_000,
               "Bob gets same haircut rate as Alice");
    assert_eq!(env.vault_balance(), 0);
}

#[test]
fn test_proportional_withdrawal_unequal_positions_defense_in_depth() {
    // Same as above but with unequal positions. Verify the haircut % is
    // the same for all depositors regardless of size.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.stake(&alice, 3_000_000); // 75% of pool
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);   // 25% of pool
    // vault = 4M, total_staked = 4M

    // Directly set vault to 3M (75% funded)
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let vault_acct = env.svm.get_account(&stake_vault).unwrap();
    let (mrc_pda, _) = Pubkey::find_program_address(
        &[b"mrc", env.slab.as_ref()], &env.rewards_id);
    env.svm.set_account(stake_vault, Account {
        lamports: vault_acct.lamports,
        data: make_token_account_data(&env.collateral_mint, &mrc_pda, 3_000_000),
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    env.set_clock(200);

    // Alice: 3M * 3M/4M = 2.25M
    let (alice_col, _) = env.unstake_and_get_atas(&alice, 3_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 2_250_000);

    // Bob: 1M * 750K/1M = 750K (vault after Alice = 3M - 2.25M = 750K)
    let (bob_col, _) = env.unstake_and_get_atas(&bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 750_000);

    // Both took a 25% haircut. Total returned = 2.25M + 750K = 3M = vault. ✓
    assert_eq!(env.vault_balance(), 0);
}

#[test]
fn test_proportional_partial_withdrawal_defense_in_depth() {
    // Partial withdrawal with underfunded vault. Verify math is correct
    // across multiple partial withdrawals.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    // Set vault to 600K (60% funded)
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let vault_acct = env.svm.get_account(&stake_vault).unwrap();
    let (mrc_pda, _) = Pubkey::find_program_address(
        &[b"mrc", env.slab.as_ref()], &env.rewards_id);
    env.svm.set_account(stake_vault, Account {
        lamports: vault_acct.lamports,
        data: make_token_account_data(&env.collateral_mint, &mrc_pda, 600_000),
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    env.set_clock(200);

    // Partial: withdraw 400K accounting units
    // actual = 400K * 600K / 1M = 240K
    let (col_1, _) = env.unstake_and_get_atas(&user, 400_000);
    assert_eq!(env.read_token_balance(&col_1), 240_000);
    // vault = 360K, total_staked = 600K

    // Withdraw remaining 600K
    // actual = 600K * 360K / 600K = 360K
    env.advance_blockhash();
    let (col_2, _) = env.unstake_and_get_atas(&user, 600_000);
    assert_eq!(env.read_token_balance(&col_2), 360_000);

    // Total: 240K + 360K = 600K = original vault balance. ✓
    assert_eq!(env.vault_balance(), 0);
}

#[test]
fn test_proportional_full_drain_defense_in_depth() {
    // Vault set to 0 via direct manipulation. All users must still be
    // able to withdraw (get 0 collateral) without revert.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    env.stake(&user, 1_000_000);

    // Vault set to 0
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", env.slab.as_ref()], &env.rewards_id);
    let vault_acct = env.svm.get_account(&stake_vault).unwrap();
    let (mrc_pda, _) = Pubkey::find_program_address(
        &[b"mrc", env.slab.as_ref()], &env.rewards_id);
    env.svm.set_account(stake_vault, Account {
        lamports: vault_acct.lamports,
        data: make_token_account_data(&env.collateral_mint, &mrc_pda, 0),
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    env.set_clock(200);
    let (col_ata, coin_ata) = env.unstake_and_get_atas(&user, 1_000_000);
    assert_eq!(env.read_token_balance(&col_ata), 0, "0 collateral from empty vault");
    // COIN rewards still minted
    let coin_bal = env.read_token_balance(&coin_ata);
    assert!(coin_bal >= 999 && coin_bal <= 1000, "COIN must still mint");
}

// ============================================================================
// Tests: market isolation — risk and capital flow are per-market only
// ============================================================================

#[test]
fn test_isolation_draw_from_market_a_does_not_touch_market_b() {
    // Two markets share one COIN. Inject profit only into Market A.
    // DAO draws from A. Verify Market B's vault balance is exactly unchanged.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100); // market 1 (env.slab)
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Alice deposits in market 1, Bob in market 2 — same amounts
    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);
    assert_eq!(env.vault_balance_for(&slab1), 1_000_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000);

    // Inject 500K profit into MARKET 1 ONLY
    env.inject_profit(&slab1, 500_000);
    assert_eq!(env.vault_balance_for(&slab1), 1_500_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000, "Market 2 untouched by injection");

    // DAO draws 500K profit from market 1
    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.draw_insurance_from(&slab1, 500_000, &dao_dest);

    // CRITICAL ASSERTIONS: Market 2 vault balance must be exactly unchanged
    assert_eq!(env.vault_balance_for(&slab1), 1_000_000, "Market 1 reduced by draw");
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000, "Market 2 vault must be untouched");
    assert_eq!(env.read_token_balance(&dao_dest), 500_000, "DAO received exactly 500K");

    // Both depositors get their full deposit back
    env.set_clock(200);
    let (alice_col, _) = env.unstake_in_get_atas(&slab1, &alice, 1_000_000);
    let (bob_col, _) = env.unstake_in_get_atas(&slab2, &bob, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);
}

#[test]
fn test_isolation_dao_cannot_draw_from_market_b_via_market_a_profit() {
    // Market A has profit, Market B has no profit. DAO must not be able to
    // drain Market B's vault using Market A's "profit budget".
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);

    // Inject 500K profit ONLY in market 1
    env.inject_profit(&slab1, 500_000);
    // market 1: vault=1.5M, total_staked=1M, profit=500K
    // market 2: vault=1M, total_staked=1M, profit=0

    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);

    // DAO tries to draw from market 2 (which has 0 profit). Must fail.
    let result = env.try_draw_insurance_from(&slab2, 1, &dao_dest);
    assert!(result.is_err(), "Cannot draw from market 2 (no profit)");
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000, "Market 2 vault untouched");

    // DAO can still draw from market 1 (it has profit).
    env.draw_insurance_from(&slab1, 500_000, &dao_dest);
    assert_eq!(env.vault_balance_for(&slab1), 1_000_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000);
}

#[test]
fn test_isolation_cross_market_attack_wrong_mrc_with_other_vault() {
    // Attacker (or DAO) tries to drain market B's vault by passing market A's MRC
    // along with market B's vault. Must be rejected.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);
    env.inject_profit(&slab1, 500_000); // only market 1 has profit

    // Build a malicious draw: market 1's MRC + slab + coin_cfg, but market 2's vault
    let (mrc_pda1, _) = Pubkey::find_program_address(
        &[b"mrc", slab1.as_ref()], &env.rewards_id);
    let (coin_cfg_pda, _) = Pubkey::find_program_address(
        &[b"coin_cfg", env.coin_mint.as_ref()], &env.rewards_id);
    let (vault2, _) = Pubkey::find_program_address(
        &[b"stake_vault", slab2.as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);

    let mut data = vec![3u8]; // governance adapter IX_DRAW_INSURANCE
    data.extend_from_slice(&500_000u64.to_le_bytes());

    let ix = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(env.dao_authority.pubkey(), true),
            AccountMeta::new(env.governance_authority_pda, false),
            AccountMeta::new_readonly(env.rewards_id, false),
            AccountMeta::new_readonly(mrc_pda1, false), // market 1's MRC
            AccountMeta::new_readonly(slab1, false),    // market 1's slab
            AccountMeta::new(vault2, false),            // market 2's VAULT (mismatch!)
            AccountMeta::new(dao_dest, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data,
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority], env.svm.latest_blockhash());
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Cross-market vault substitution must be rejected");

    // Both vaults must be unchanged
    assert_eq!(env.vault_balance_for(&slab1), 1_500_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000);
    assert_eq!(env.read_token_balance(&dao_dest), 0, "Attacker got nothing");
}

#[test]
fn test_isolation_alice_two_market_positions_independent() {
    // Same user (Alice) deposits in two markets. Verify positions are tracked
    // independently — withdrawing from one does NOT affect the other.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &alice, 2_000_000);

    // Verify both positions exist independently
    let (sp1, _) = Pubkey::find_program_address(
        &[b"sp", slab1.as_ref(), alice.pubkey().as_ref()], &env.rewards_id);
    let (sp2, _) = Pubkey::find_program_address(
        &[b"sp", slab2.as_ref(), alice.pubkey().as_ref()], &env.rewards_id);
    assert_ne!(sp1, sp2, "Alice has two distinct SP PDAs (one per market)");

    let sp1_data = env.svm.get_account(&sp1).unwrap();
    let sp2_data = env.svm.get_account(&sp2).unwrap();
    let amt1 = u64::from_le_bytes(sp1_data.data[8..16].try_into().unwrap());
    let amt2 = u64::from_le_bytes(sp2_data.data[8..16].try_into().unwrap());
    assert_eq!(amt1, 1_000_000, "Market 1 position");
    assert_eq!(amt2, 2_000_000, "Market 2 position");

    // Alice withdraws fully from market 1
    env.set_clock(200);
    let (alice_col1, _) = env.unstake_in_get_atas(&slab1, &alice, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col1), 1_000_000);

    // Verify market 2 position is untouched
    let sp2_after = env.svm.get_account(&sp2).unwrap();
    let amt2_after = u64::from_le_bytes(sp2_after.data[8..16].try_into().unwrap());
    assert_eq!(amt2_after, 2_000_000, "Market 2 position untouched by market 1 withdraw");
    assert_eq!(env.vault_balance_for(&slab2), 2_000_000, "Market 2 vault untouched");

    // Alice withdraws fully from market 2
    let (alice_col2, _) = env.unstake_in_get_atas(&slab2, &alice, 2_000_000);
    assert_eq!(env.read_token_balance(&alice_col2), 2_000_000);
}

#[test]
fn test_isolation_market_a_drained_does_not_haircut_market_b() {
    // Defense-in-depth: even if market A's vault somehow goes underfunded,
    // market B depositors must get their full deposits back.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);

    // Directly drain market 1's vault (simulating defense-in-depth scenario)
    let (vault1, _) = Pubkey::find_program_address(
        &[b"stake_vault", slab1.as_ref()], &env.rewards_id);
    let (mrc_pda1, _) = Pubkey::find_program_address(
        &[b"mrc", slab1.as_ref()], &env.rewards_id);
    let vault1_acct = env.svm.get_account(&vault1).unwrap();
    env.svm.set_account(vault1, Account {
        lamports: vault1_acct.lamports,
        data: make_token_account_data(&env.collateral_mint, &mrc_pda1, 500_000), // 50% drained
        owner: spl_token::ID,
        executable: false,
        rent_epoch: 0,
    }).unwrap();

    assert_eq!(env.vault_balance_for(&slab1), 500_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000, "Market 2 untouched by manipulation");

    // Alice gets 50% (proportional defense-in-depth in market 1)
    env.set_clock(200);
    let (alice_col, _) = env.unstake_in_get_atas(&slab1, &alice, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 500_000, "Alice: 50% of drained vault 1");

    // Bob gets 100% — market 2 is unaffected
    let (bob_col, _) = env.unstake_in_get_atas(&slab2, &bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000, "Bob: 100% — market 2 isolated");
}

#[test]
fn test_isolation_per_market_profit_calculation() {
    // Verify the profit formula is computed against the SAME market's MRC.
    // Drawing X from market A is bounded by (vault_A - total_staked_A),
    // not by any other market's surplus.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);
    env.inject_profit(&slab1, 1_000_000); // market 1 has 1M profit; market 2 has 0

    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);

    // Try to draw 1M from market 2 — must fail (market 2 has no profit, regardless of market 1)
    let result = env.try_draw_insurance_from(&slab2, 1_000_000, &dao_dest);
    assert!(result.is_err(), "Market 2 has 0 profit; cannot draw using market 1's surplus");

    // Try to draw 1 token from market 2 — must also fail
    let result = env.try_draw_insurance_from(&slab2, 1, &dao_dest);
    assert!(result.is_err(), "Cannot draw any amount from market 2");

    // Market 1 draw works for up to its own profit
    env.draw_insurance_from(&slab1, 1_000_000, &dao_dest);
    // Subsequent draw from market 1 should also fail (profit exhausted)
    env.advance_blockhash();
    let result = env.try_draw_insurance_from(&slab1, 1, &dao_dest);
    assert!(result.is_err(), "Market 1 profit exhausted");

    assert_eq!(env.read_token_balance(&dao_dest), 1_000_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000, "Market 2 vault untouched");
}

#[test]
fn test_isolation_unstake_wrong_market_vault_rejected() {
    // User tries to unstake from market 1 but passes market 2's vault.
    // The vault PDA check must reject this.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);

    // Alice tries to unstake from market 1 but with market 2's vault
    let (mrc_pda1, _) = Pubkey::find_program_address(
        &[b"mrc", slab1.as_ref()], &env.rewards_id);
    let (vault2, _) = Pubkey::find_program_address(
        &[b"stake_vault", slab2.as_ref()], &env.rewards_id);
    let (sp1, _) = Pubkey::find_program_address(
        &[b"sp", slab1.as_ref(), alice.pubkey().as_ref()], &env.rewards_id);
    let col_mint = env.collateral_mint;
    let alice_col = env.create_ata(&col_mint, &alice.pubkey(), 0);
    let alice_coin = env.create_coin_ata(&alice.pubkey(), 0);

    env.set_clock(200);
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new(mrc_pda1, false),
            AccountMeta::new_readonly(slab1, false),
            AccountMeta::new(alice_col, false),
            AccountMeta::new(vault2, false), // WRONG vault (market 2)
            AccountMeta::new(sp1, false),
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new(alice_coin, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_unstake(1_000_000),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&alice.pubkey()),
        &[&alice], env.svm.latest_blockhash());
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "Cross-market vault substitution must be rejected");

    // Vaults unchanged
    assert_eq!(env.vault_balance_for(&slab1), 1_000_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 0, "Alice received nothing");
}

#[test]
fn test_isolation_market_a_loss_does_not_change_market_b_total_staked() {
    // Verify that the MRC of one market is not modified by operations on another.
    // Specifically, total_staked accounting is per-market.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 3_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);

    // Read total_staked for both markets
    let (mrc1, _) = Pubkey::find_program_address(
        &[b"mrc", slab1.as_ref()], &env.rewards_id);
    let (mrc2, _) = Pubkey::find_program_address(
        &[b"mrc", slab2.as_ref()], &env.rewards_id);
    let mrc1_data = env.svm.get_account(&mrc1).unwrap();
    let mrc2_data = env.svm.get_account(&mrc2).unwrap();
    let total1_before = u64::from_le_bytes(mrc1_data.data[152..160].try_into().unwrap());
    let total2_before = u64::from_le_bytes(mrc2_data.data[152..160].try_into().unwrap());
    assert_eq!(total1_before, 3_000_000);
    assert_eq!(total2_before, 1_000_000);

    // Alice withdraws fully from market 1
    env.set_clock(200);
    env.unstake_in_get_atas(&slab1, &alice, 3_000_000);

    // Re-read both. Market 1 total goes to 0; Market 2 unchanged.
    let mrc1_after = env.svm.get_account(&mrc1).unwrap();
    let mrc2_after = env.svm.get_account(&mrc2).unwrap();
    let total1_after = u64::from_le_bytes(mrc1_after.data[152..160].try_into().unwrap());
    let total2_after = u64::from_le_bytes(mrc2_after.data[152..160].try_into().unwrap());
    assert_eq!(total1_after, 0, "Market 1 fully withdrawn");
    assert_eq!(total2_after, 1_000_000, "Market 2 total_staked must be unchanged");
}

#[test]
fn test_isolation_dao_can_only_drain_after_local_market_depositors_exit() {
    // After all market 1 depositors exit (total_staked_1 == 0), DAO can drain
    // market 1's remaining vault. Market 2 is unaffected.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    let slab1 = env.slab;
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);

    env.inject_profit(&slab1, 200_000); // market 1 has 200K profit
    env.inject_profit(&slab2, 100_000); // market 2 has 100K profit

    // Alice withdraws from market 1 first
    env.set_clock(200);
    env.unstake_in_get_atas(&slab1, &alice, 1_000_000);
    // Market 1: vault = 200K (only profit left), total_staked = 0
    assert_eq!(env.vault_balance_for(&slab1), 200_000);

    // DAO can drain all 200K from market 1 (depositors gone)
    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.draw_insurance_from(&slab1, 200_000, &dao_dest);
    assert_eq!(env.vault_balance_for(&slab1), 0);

    // BUT market 2 still has Bob's deposit. DAO can only draw market 2's profit.
    // Market 2: vault=1.1M, total_staked=1M, profit=100K
    let result = env.try_draw_insurance_from(&slab2, 200_000, &dao_dest);
    assert!(result.is_err(), "Market 2: can only draw 100K profit, not 200K");
    env.draw_insurance_from(&slab2, 100_000, &dao_dest);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000, "Market 2: only profit drawn");

    // Bob still gets his full deposit
    let (bob_col, _) = env.unstake_in_get_atas(&slab2, &bob, 1_000_000);
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000);
}

// ============================================================================
// Tests: end-to-end insurance integration (real on-chain CPI path)
//
// These tests exercise the full wire between our program and the percolator
// insurance fund:
//   1. Bootstrap registers MRC PDA as percolator insurance_operator.
//   2. Fees accrue on percolator's side (simulated via permissionless TopUpInsurance).
//   3. Our program's pull_insurance CPIs percolator's WithdrawInsuranceLimited
//      (signed by MRC PDA as operator) to capture fees into the stake_vault.
//   4. DAO's draw_insurance extracts the resulting profit.
//   5. User unstake is unaffected and drains from the stake_vault.
// ============================================================================

#[test]
fn test_e2e_register_insurance_operator_sets_header() {
    // Verify the bootstrap ceremony actually updated the percolator header.
    let mut env = TestEnv::new();
    let (mrc_pda, _) = Pubkey::find_program_address(
        &[b"mrc", env.slab.as_ref()], &env.rewards_id);

    // Before init_market_rewards, admin is still self.payer.
    let header_pre = percolator_prog::state::read_header(
        &env.svm.get_account(&env.slab).unwrap().data);
    assert_eq!(Pubkey::new_from_array(header_pre.admin), env.payer.pubkey());
    assert_eq!(Pubkey::new_from_array(header_pre.insurance_operator), env.payer.pubkey(),
        "before registration, operator defaults to admin");

    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    // After: operator should be MRC PDA, admin burned.
    let header_post = percolator_prog::state::read_header(
        &env.svm.get_account(&env.slab).unwrap().data);
    assert_eq!(header_post.admin, [0u8; 32], "admin burned");
    assert_eq!(Pubkey::new_from_array(header_post.insurance_operator), mrc_pda,
        "insurance_operator = MRC PDA");
}

#[test]
fn test_e2e_pull_insurance_succeeds_when_operator_registered() {
    // Full happy path: donor pushes funds into percolator insurance, our
    // pull_insurance pulls them out via the operator PDA path.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    // Need a staker so the stake_vault has a legit origin and we can verify
    // the pulled funds become drawable profit (not depositor capital).
    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.stake(&alice, 1_000_000);

    // Donor pushes 500K into percolator's insurance_fund (simulates earned fees).
    assert_eq!(env.percolator_insurance_balance(&env.slab.clone()), 0);
    let slab = env.slab;
    env.topup_percolator_insurance(&slab, 500_000);
    assert_eq!(env.percolator_insurance_balance(&env.slab.clone()), 500_000);

    // Pre-pull: stake_vault has exactly the staker's deposit.
    assert_eq!(env.vault_balance(), 1_000_000);

    // Pull 500K from percolator → our stake_vault via WithdrawInsuranceLimited CPI.
    env.set_clock(200); // ensure cooldown not an issue (first call anyway)
    env.pull_insurance(&slab, 500_000);

    // stake_vault grew by exactly 500K; percolator insurance drained.
    assert_eq!(env.vault_balance(), 1_500_000);
    assert_eq!(env.percolator_insurance_balance(&slab), 0);
}

#[test]
fn test_e2e_pull_then_draw_extracts_real_profit_to_dao() {
    // Full profit-extraction flow: stake → fees → pull → draw.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.stake(&alice, 1_000_000);
    // vault=1M, total_staked=1M, no profit yet

    let slab = env.slab;
    env.topup_percolator_insurance(&slab, 300_000);
    env.set_clock(200);
    env.pull_insurance(&slab, 300_000);
    // vault=1.3M, total_staked=1M, profit=300K

    // DAO draws profit via our existing draw_insurance (local vault path).
    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.advance_blockhash();
    env.draw_insurance(300_000, &dao_dest);

    assert_eq!(env.read_token_balance(&dao_dest), 300_000, "DAO extracted real profit");
    assert_eq!(env.vault_balance(), 1_000_000, "vault back to depositor capital");
    assert_eq!(env.percolator_insurance_balance(&slab), 0);

    // Depositor capital is still intact — Alice can withdraw full 1M.
    env.advance_blockhash();
    let (alice_col, _) = env.unstake_and_get_atas(&alice, 1_000_000);
    assert_eq!(env.read_token_balance(&alice_col), 1_000_000);
}

#[test]
fn test_e2e_pull_insurance_requires_operator_pda() {
    // Negative test: if we somehow attempt to pull without the MRC PDA being
    // the operator, the percolator CPI must reject with Custom(10) (UnauthorizedAdmin-like).
    // Achieve this by creating a market where we SKIP register_insurance_operator.
    let mut env = TestEnv::new();
    env.init_coin_config();

    // Manually init market rewards WITHOUT registering operator first.
    env.burn_market_admin();
    let slab = env.slab;
    env.try_init_market_rewards_for_slab(&slab, 1000, 100)
        .expect("init rewards ok");

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.stake(&alice, 1_000_000);
    env.topup_percolator_insurance(&slab, 500_000);

    env.set_clock(200);
    let result = env.try_pull_insurance(&slab, 500_000);
    assert!(result.is_err(), "pull must fail when MRC PDA is not insurance_operator");
    // Percolator insurance balance unchanged
    assert_eq!(env.percolator_insurance_balance(&slab), 500_000);
    // Our vault unchanged
    assert_eq!(env.vault_balance(), 1_000_000);
}

#[test]
fn test_e2e_pull_insurance_respects_cooldown() {
    // Our InitMarket sets cooldown = 1 slot. Back-to-back pulls in the same
    // slot should be rejected by percolator's WithdrawInsuranceLimited.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.stake(&alice, 1_000_000);

    let slab = env.slab;
    env.topup_percolator_insurance(&slab, 100);
    env.set_clock(200);
    env.pull_insurance(&slab, 50);

    // Second pull in same slot: should fail cooldown check (1 slot required).
    env.advance_blockhash();
    let result = env.try_pull_insurance(&slab, 50);
    assert!(result.is_err(), "back-to-back pull must hit cooldown");

    // Advance slot by 1 → now it works.
    env.set_clock(201);
    env.pull_insurance(&slab, 50);
    assert_eq!(env.vault_balance(), 1_000_100);
}

#[test]
fn test_e2e_multi_market_pull_isolation() {
    // Two markets, each with its own MRC PDA as operator. Fees accrue on A
    // only. Pull from A does not affect B's insurance balance, and B's operator
    // (its own MRC PDA) is distinct.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);
    let slab1 = env.slab;

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);

    env.topup_percolator_insurance(&slab1, 200_000);
    assert_eq!(env.percolator_insurance_balance(&slab1), 200_000);
    assert_eq!(env.percolator_insurance_balance(&slab2), 0);

    env.set_clock(200);
    env.pull_insurance(&slab1, 200_000);

    // Market 1's insurance drained; Market 2 untouched.
    assert_eq!(env.percolator_insurance_balance(&slab1), 0);
    assert_eq!(env.percolator_insurance_balance(&slab2), 0);
    assert_eq!(env.vault_balance_for(&slab1), 1_200_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000, "market 2 vault untouched");

    // Verify each market's operator is its own MRC PDA.
    let (mrc1, _) = Pubkey::find_program_address(&[b"mrc", slab1.as_ref()], &env.rewards_id);
    let (mrc2, _) = Pubkey::find_program_address(&[b"mrc", slab2.as_ref()], &env.rewards_id);
    let h1 = percolator_prog::state::read_header(&env.svm.get_account(&slab1).unwrap().data);
    let h2 = percolator_prog::state::read_header(&env.svm.get_account(&slab2).unwrap().data);
    assert_eq!(Pubkey::new_from_array(h1.insurance_operator), mrc1);
    assert_eq!(Pubkey::new_from_array(h2.insurance_operator), mrc2);
    assert_ne!(mrc1, mrc2);
}

#[test]
fn test_e2e_cross_market_cannot_pull_using_wrong_mrc() {
    // Attack: pass Market A's MRC PDA as operator but Market B's vault/slab.
    // Must fail at either our PDA check or percolator's operator check.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);
    let slab2 = env.init_second_market(1000, 100);
    let slab1 = env.slab;

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.stake_in(&slab1, &alice, 1_000_000);
    env.stake_in(&slab2, &bob, 1_000_000);

    // Put fees in market 2's insurance
    env.topup_percolator_insurance(&slab2, 500_000);
    env.set_clock(200);

    // Build a malicious pull: market 1's MRC as operator, market 2's vault as source
    let (mrc1, _) = Pubkey::find_program_address(&[b"mrc", slab1.as_ref()], &env.rewards_id);
    let (stake_vault1, _) = Pubkey::find_program_address(
        &[b"stake_vault", slab1.as_ref()], &env.rewards_id);
    let slab2_data = env.svm.get_account(&slab2).unwrap().data;
    let config2 = percolator_prog::state::read_config(&slab2_data);
    let perc_vault2 = Pubkey::new_from_array(config2.vault_pubkey);
    let (perc_vault_pda2, _) = Pubkey::find_program_address(
        &[b"vault", slab2.as_ref()], &env.percolator_id);

    let mut data = vec![7u8]; // IX_PULL_INSURANCE
    data.extend_from_slice(&500_000u64.to_le_bytes());

    let payer = Keypair::new();
    env.svm.airdrop(&payer.pubkey(), 1_000_000_000).unwrap();

    // Pass market 1's MRC but market 2's slab/vault
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(mrc1, false),        // Market 1 MRC
            AccountMeta::new(slab2, false),                 // Market 2 slab
            AccountMeta::new(stake_vault1, false),          // Market 1 stake_vault
            AccountMeta::new(perc_vault2, false),           // Market 2 percolator vault
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(perc_vault_pda2, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(env.percolator_id, false),
        ],
        data,
    };
    let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(400_000);
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix, ix], Some(&payer.pubkey()),
        &[&payer], env.svm.latest_blockhash());
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "cross-market pull must be rejected");

    // Both markets unchanged
    assert_eq!(env.percolator_insurance_balance(&slab2), 500_000);
    assert_eq!(env.vault_balance_for(&slab1), 1_000_000);
    assert_eq!(env.vault_balance_for(&slab2), 1_000_000);
}

#[test]
fn test_e2e_full_lifecycle_with_real_cpi() {
    // End-to-end: stake, fees accrue, pull, COIN yield earned, DAO draws profit,
    // users unstake with full capital + rewards.
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Epoch 1: Alice + Bob stake equal amounts
    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);
    let slab = env.slab;

    // Simulate trading fees accruing over time
    env.set_clock(150);
    env.topup_percolator_insurance(&slab, 200_000); // first batch of fees
    env.set_clock(200);
    env.pull_insurance(&slab, 200_000);

    env.set_clock(250);
    env.topup_percolator_insurance(&slab, 300_000); // second batch
    env.set_clock(300);
    env.pull_insurance(&slab, 300_000);

    // Total fees captured: 500K. vault = 2M (deposits) + 500K (profit) = 2.5M.
    assert_eq!(env.vault_balance(), 2_500_000);
    assert_eq!(env.percolator_insurance_balance(&slab), 0);

    // DAO draws profit
    let col_mint = env.collateral_mint;
    let dao_dest = env.create_ata(&col_mint, &env.dao_authority.pubkey(), 0);
    env.advance_blockhash();
    env.draw_insurance(500_000, &dao_dest);
    assert_eq!(env.read_token_balance(&dao_dest), 500_000);
    assert_eq!(env.vault_balance(), 2_000_000, "only depositor capital left");

    // DAO cannot draw more (all profit extracted)
    env.advance_blockhash();
    let result = env.try_draw_insurance(1, &dao_dest);
    assert!(result.is_err(), "no more profit to draw");

    // Users withdraw full deposits + accumulated COIN
    env.set_clock(400);
    let (alice_col, alice_coin) = env.unstake_and_get_atas(&alice, 1_000_000);
    let (bob_col, bob_coin) = env.unstake_and_get_atas(&bob, 1_000_000);

    assert_eq!(env.read_token_balance(&alice_col), 1_000_000, "Alice: full deposit");
    assert_eq!(env.read_token_balance(&bob_col), 1_000_000, "Bob: full deposit");

    // COIN rewards: over ~3 epochs (slot 100 → 400), 1000 × 3 = 3000 total COIN
    let alice_c = env.read_token_balance(&alice_coin);
    let bob_c = env.read_token_balance(&bob_coin);
    let total_coin = alice_c + bob_c;
    assert!(total_coin >= 2998 && total_coin <= 3000,
        "Total COIN ~3000, got {}", total_coin);

    // Final state: everything drained
    assert_eq!(env.vault_balance(), 0);
}
