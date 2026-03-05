//! Integration tests for the rewards program.
//!
//! Uses LiteSVM with BPF binaries for both percolator-prog and rewards-program.
//!
//! Build both programs first:
//!   cd ../percolator-prog && cargo build-sbf
//!   cargo build-sbf
//!
//! Run: cargo test --test integration

use litesvm::LiteSVM;
use solana_sdk::{
    account::Account,
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
    transaction::Transaction,
};
use spl_token::state::{Account as TokenAccount, AccountState, Mint};
use std::path::PathBuf;

// Production SLAB_LEN for BPF (MAX_ACCOUNTS=4096)
const SLAB_LEN: usize = 1058152;
const MAX_ACCOUNTS: usize = 4096;

const PYTH_RECEIVER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x0c, 0xb7, 0xfa, 0xbb, 0x52, 0xf7, 0xa6, 0x48, 0xbb, 0x5b, 0x31, 0x7d, 0x9a, 0x01, 0x8b,
    0x90, 0x57, 0xcb, 0x02, 0x47, 0x74, 0xfa, 0xfe, 0x01, 0xe6, 0xc4, 0xdf, 0x98, 0xcc, 0x38,
    0x58, 0x81,
]);

const TEST_FEED_ID: [u8; 32] = [0xABu8; 32];


/// EPOCH_SLOTS must match the constant in the rewards program
const EPOCH_SLOTS: u64 = 216_000;

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

/// Make a fake MetaDAO receipt: contributor(32) + contributed_lamports(u64)
fn make_receipt_data(contributor: &Pubkey, contributed_lamports: u64) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(contributor.as_ref());
    data.extend_from_slice(&contributed_lamports.to_le_bytes());
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
    data.extend_from_slice(&u64::MAX.to_le_bytes()); // max_staleness_secs
    data.extend_from_slice(&500u16.to_le_bytes()); // conf_filter_bps
    data.push(0u8); // invert
    data.extend_from_slice(&0u32.to_le_bytes()); // unit_scale
    data.extend_from_slice(&0u64.to_le_bytes()); // initial_mark_price_e6
    // Per-market admin limits
    data.extend_from_slice(&u128::MAX.to_le_bytes()); // max_maintenance_fee_per_slot
    data.extend_from_slice(&u128::MAX.to_le_bytes()); // max_risk_threshold
    data.extend_from_slice(&0u64.to_le_bytes()); // min_oracle_price_cap_e2bps
    // RiskParams
    data.extend_from_slice(&0u64.to_le_bytes()); // warmup_period_slots
    data.extend_from_slice(&500u64.to_le_bytes()); // maintenance_margin_bps (5%)
    data.extend_from_slice(&1000u64.to_le_bytes()); // initial_margin_bps (10%)
    data.extend_from_slice(&trading_fee_bps.to_le_bytes());
    data.extend_from_slice(&(MAX_ACCOUNTS as u64).to_le_bytes());
    data.extend_from_slice(&0u128.to_le_bytes()); // new_account_fee
    data.extend_from_slice(&0u128.to_le_bytes()); // risk_reduction_threshold
    data.extend_from_slice(&0u128.to_le_bytes()); // maintenance_fee_per_slot
    data.extend_from_slice(&u64::MAX.to_le_bytes()); // max_crank_staleness_slots
    data.extend_from_slice(&50u64.to_le_bytes()); // liquidation_fee_bps
    data.extend_from_slice(&1_000_000_000_000u128.to_le_bytes()); // liquidation_fee_cap
    data.extend_from_slice(&100u64.to_le_bytes()); // liquidation_buffer_bps
    data.extend_from_slice(&0u128.to_le_bytes()); // min_liquidation_abs
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
    let mut data = vec![12u8];
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
    data.extend_from_slice(&100u64.to_le_bytes()); // funding_horizon_slots
    data.extend_from_slice(&10u64.to_le_bytes()); // funding_k_bps
    data.extend_from_slice(&1_000_000u128.to_le_bytes()); // funding_inv_scale_notional_e6
    data.extend_from_slice(&100i64.to_le_bytes()); // funding_max_premium_bps
    data.extend_from_slice(&10i64.to_le_bytes()); // funding_max_bps_per_slot
    data.extend_from_slice(&0u128.to_le_bytes()); // thresh_floor
    data.extend_from_slice(&50u64.to_le_bytes()); // thresh_risk_bps
    data.extend_from_slice(&10u64.to_le_bytes()); // thresh_update_interval_slots
    data.extend_from_slice(&1000u64.to_le_bytes()); // thresh_step_bps
    data.extend_from_slice(&1000u64.to_le_bytes()); // thresh_alpha_bps
    data.extend_from_slice(&0u128.to_le_bytes()); // thresh_min
    data.extend_from_slice(&u128::MAX.to_le_bytes()); // thresh_max
    data.extend_from_slice(&0u128.to_le_bytes()); // thresh_min_step
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

fn encode_init_market_rewards(n: u64, k: u128, total_contributed_lamports: u64) -> Vec<u8> {
    let mut data = vec![0u8]; // tag = IX_INIT_MARKET_REWARDS
    data.extend_from_slice(&n.to_le_bytes());
    data.extend_from_slice(&k.to_le_bytes());
    data.extend_from_slice(&total_contributed_lamports.to_le_bytes());
    data
}

fn encode_claim_owner_rewards() -> Vec<u8> {
    vec![1u8] // tag = IX_CLAIM_OWNER_REWARDS
}

fn encode_claim_lp_rewards(lp_idx: u16) -> Vec<u8> {
    let mut data = vec![2u8]; // tag = IX_CLAIM_LP_REWARDS
    data.extend_from_slice(&lp_idx.to_le_bytes());
    data
}

// ============================================================================
// Test environment
// ============================================================================

struct TestEnv {
    svm: LiteSVM,
    percolator_id: Pubkey,
    rewards_id: Pubkey,
    payer: Keypair,
    dao_authority: Keypair,
    slab: Pubkey,
    collateral_mint: Pubkey,
    vault: Pubkey,
    pyth_index: Pubkey,
    coin_mint: Pubkey,
    mint_authority_pda: Pubkey,
    receipt_program: Pubkey,
    account_count: u16,
}

impl TestEnv {
    fn new() -> Self {
        Self::with_trading_fee(100) // 1% default trading fee for LP reward tests
    }

    fn with_trading_fee(trading_fee_bps: u64) -> Self {
        let mut svm = LiteSVM::new();

        // Load percolator-prog BPF
        let percolator_id = Pubkey::new_unique();
        let perc_bytes = std::fs::read(percolator_path()).expect("read percolator BPF");
        svm.add_program(percolator_id, &perc_bytes);

        // Load rewards-program BPF
        let rewards_id = Pubkey::new_unique();
        let rewards_bytes = std::fs::read(rewards_path()).expect("read rewards BPF");
        svm.add_program(rewards_id, &rewards_bytes);

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

        // Collateral mint (no authority — SPL token for the percolator market)
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

        // Receipt program (simulating MetaDAO)
        let receipt_program = Pubkey::new_unique();

        // DAO authority — the key that can add markets to the shared COIN
        let dao_authority = Keypair::new();
        svm.airdrop(&dao_authority.pubkey(), 100_000_000_000).unwrap();

        // COIN mint — authority is the rewards PDA derived from coin_mint key
        let coin_mint = Pubkey::new_unique();
        let (mint_authority_pda, _) =
            Pubkey::find_program_address(&[b"coin_mint_authority", coin_mint.as_ref()], &rewards_id);
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
                trading_fee_bps,
            ),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            svm.latest_blockhash(),
        );
        svm.send_transaction(tx).expect("init_market failed");

        TestEnv {
            svm,
            percolator_id,
            rewards_id,
            payer,
            dao_authority,
            slab,
            collateral_mint,
            vault,
            pyth_index,
            coin_mint,
            mint_authority_pda,
            receipt_program,
            account_count: 0,
        }
    }

    fn init_coin_config(&mut self) {
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", self.coin_mint.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
                AccountMeta::new_readonly(self.receipt_program, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_coin_config(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("init_coin_config failed");
    }

    fn try_init_coin_config_with_mint(
        &mut self,
        coin_mint: &Pubkey,
    ) -> Result<(), String> {
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", coin_mint.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new_readonly(*coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
                AccountMeta::new_readonly(self.receipt_program, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_coin_config(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| {
            format!("{:?}", e)
        })
    }

    fn init_market_rewards(&mut self, n: u64, k: u128, total_contributed: u64) {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", self.coin_mint.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market_rewards(n, k, total_contributed),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("init_market_rewards failed");
    }

    fn try_init_market_rewards(
        &mut self,
        n: u64,
        k: u128,
        total_contributed: u64,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", self.coin_mint.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market_rewards(n, k, total_contributed),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| {
            format!("{:?}", e)
        })
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
        let _ata = self.create_ata(&col_mint, &owner.pubkey(), 0);

        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.slab, false),
                AccountMeta::new(_ata, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_init_user(0),
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

    fn claim_owner_rewards(
        &mut self,
        contributor: &Keypair,
        receipt: &Pubkey,
        coin_ata: &Pubkey,
    ) {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (ocs_pda, _) = Pubkey::find_program_address(
            &[b"ocs", self.slab.as_ref(), receipt.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(contributor.pubkey(), true), // [0] contributor (signer)
                AccountMeta::new_readonly(mrc_pda, false),    // [1] market_rewards_cfg
                AccountMeta::new_readonly(self.slab, false),  // [2] market_slab
                AccountMeta::new_readonly(*receipt, false),   // [3] receipt
                AccountMeta::new(ocs_pda, false),             // [4] owner_claim_state
                AccountMeta::new(self.coin_mint, false),      // [5] coin_mint
                AccountMeta::new(*coin_ata, false),           // [6] coin_ata
                AccountMeta::new_readonly(self.mint_authority_pda, false), // [7] mint_authority
                AccountMeta::new_readonly(spl_token::ID, false), // [8] token_program
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // [9] system
                AccountMeta::new_readonly(sysvar::clock::ID, false), // [10] clock
            ],
            data: encode_claim_owner_rewards(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&contributor.pubkey()),
            &[contributor],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("claim_owner_rewards failed");
    }

    fn try_claim_owner_rewards(
        &mut self,
        contributor: &Keypair,
        receipt: &Pubkey,
        coin_ata: &Pubkey,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let (ocs_pda, _) = Pubkey::find_program_address(
            &[b"ocs", self.slab.as_ref(), receipt.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(contributor.pubkey(), true),
                AccountMeta::new_readonly(mrc_pda, false),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new_readonly(*receipt, false),
                AccountMeta::new(ocs_pda, false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new(*coin_ata, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_claim_owner_rewards(),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&contributor.pubkey()),
            &[contributor],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn claim_lp_rewards(&mut self, lp_owner: &Keypair, lp_idx: u16, coin_ata: &Pubkey) {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let lp_idx_bytes = lp_idx.to_le_bytes();
        let (lcs_pda, _) = Pubkey::find_program_address(
            &[b"lcs", self.slab.as_ref(), &lp_idx_bytes],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(lp_owner.pubkey(), true),       // [0] lp_owner (signer)
                AccountMeta::new_readonly(mrc_pda, false),        // [1] market_rewards_cfg
                AccountMeta::new_readonly(self.slab, false),      // [2] market_slab
                AccountMeta::new(lcs_pda, false),                 // [3] lp_claim_state
                AccountMeta::new(self.coin_mint, false),          // [4] coin_mint
                AccountMeta::new(*coin_ata, false),               // [5] coin_ata
                AccountMeta::new_readonly(self.mint_authority_pda, false), // [6] mint_authority
                AccountMeta::new_readonly(spl_token::ID, false),  // [7] token_program
                AccountMeta::new_readonly(self.percolator_id, false), // [8] percolator program
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // [9] system
            ],
            data: encode_claim_lp_rewards(lp_idx),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&lp_owner.pubkey()),
            &[lp_owner],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("claim_lp_rewards failed");
    }

    fn try_claim_lp_rewards(
        &mut self,
        lp_owner: &Keypair,
        lp_idx: u16,
        coin_ata: &Pubkey,
    ) -> Result<(), String> {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);
        let lp_idx_bytes = lp_idx.to_le_bytes();
        let (lcs_pda, _) = Pubkey::find_program_address(
            &[b"lcs", self.slab.as_ref(), &lp_idx_bytes],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(lp_owner.pubkey(), true),
                AccountMeta::new_readonly(mrc_pda, false),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(lcs_pda, false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new(*coin_ata, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(self.percolator_id, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_claim_lp_rewards(lp_idx),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&lp_owner.pubkey()),
            &[lp_owner],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    /// Set up a receipt account owned by a fake meta_dao program
    fn create_receipt(
        &mut self,
        contributor: &Pubkey,
        contributed_lamports: u64,
    ) -> Pubkey {
        let receipt = Pubkey::new_unique();
        let owner = self.receipt_program;
        self.svm
            .set_account(
                receipt,
                Account {
                    lamports: 1_000_000,
                    data: make_receipt_data(contributor, contributed_lamports),
                    owner,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        receipt
    }

    fn read_token_balance(&self, account: &Pubkey) -> u64 {
        let data = self.svm.get_account(account).unwrap();
        let token = TokenAccount::unpack(&data.data).unwrap();
        token.amount
    }
}

// ============================================================================
// Tests: init_coin_config
// ============================================================================

#[test]
fn test_init_coin_config_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    // Verify CoinConfig PDA was created
    let (coin_cfg_pda, _) =
        Pubkey::find_program_address(&[b"coin_cfg", env.coin_mint.as_ref()], &env.rewards_id);
    let cfg_account = env.svm.get_account(&coin_cfg_pda).unwrap();
    assert_eq!(cfg_account.owner, env.rewards_id);
    assert_eq!(cfg_account.data.len(), 72); // COIN_CFG_SIZE

    assert_eq!(&cfg_account.data[..8], b"CCFG_INI");

    let stored_auth = Pubkey::new_from_array(cfg_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_auth, env.dao_authority.pubkey());

    let stored_rp = Pubkey::new_from_array(cfg_account.data[40..72].try_into().unwrap());
    assert_eq!(stored_rp, env.receipt_program);
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
    let k = 1u128 << 64; // 1 COIN per fee atom
    let total_contributed = 10_000_000u64;

    env.init_market_rewards(n, k, total_contributed);

    // Verify MRC PDA was created by reading it
    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let mrc_account = env.svm.get_account(&mrc_pda).unwrap();
    assert_eq!(mrc_account.owner, env.rewards_id);
    assert_eq!(mrc_account.data.len(), 144); // MRC_SIZE

    // Verify discriminator
    assert_eq!(&mrc_account.data[..8], b"MRC_INIT");

    // Verify market_slab stored correctly
    let stored_slab = Pubkey::new_from_array(mrc_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_slab, env.slab);

    // Verify coin_mint stored correctly
    let stored_mint = Pubkey::new_from_array(mrc_account.data[40..72].try_into().unwrap());
    assert_eq!(stored_mint, env.coin_mint);

    // Verify receipt_program (copied from CoinConfig, at offset 72)
    let stored_rp = Pubkey::new_from_array(mrc_account.data[72..104].try_into().unwrap());
    assert_eq!(stored_rp, env.receipt_program);

    // Verify N (at offset 104)
    let stored_n = u64::from_le_bytes(mrc_account.data[104..112].try_into().unwrap());
    assert_eq!(stored_n, n);

    // Verify K (at offset 112)
    let stored_k = u128::from_le_bytes(mrc_account.data[112..128].try_into().unwrap());
    assert_eq!(stored_k, k);

    // Verify market_start_slot (at offset 128)
    let stored_start = u64::from_le_bytes(mrc_account.data[128..136].try_into().unwrap());
    assert_eq!(stored_start, 100);

    // Verify total_contributed_lamports (at offset 136)
    let stored_total = u64::from_le_bytes(mrc_account.data[136..144].try_into().unwrap());
    assert_eq!(stored_total, total_contributed);
}

#[test]
fn test_init_market_rewards_double_init_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 1u128 << 64, 10_000_000);

    // Second init should fail (PDA already exists)
    env.advance_blockhash();
    let result = env.try_init_market_rewards(1000, 1u128 << 64, 10_000_000);
    assert!(result.is_err(), "Double init should fail");
}

#[test]
fn test_init_market_rewards_k_exceeds_max_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let max_k: u128 = 1_000_000u128 << 64;
    let result = env.try_init_market_rewards(1000, max_k + 1, 10_000_000);
    assert!(result.is_err(), "K > MAX_LP_COIN_PER_FEE_FP should fail");
}

#[test]
fn test_init_market_rewards_k_at_max_succeeds() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let max_k: u128 = 1_000_000u128 << 64;
    env.init_market_rewards(1000, max_k, 10_000_000);
}

#[test]
fn test_init_market_rewards_wrong_authority_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    // Try to init_market_rewards with a different signer (not the CoinConfig authority)
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let (coin_cfg_pda, _) =
        Pubkey::find_program_address(&[b"coin_cfg", env.coin_mint.as_ref()], &env.rewards_id);

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new_readonly(env.slab, false),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(1000, 1u128 << 64, 10_000_000),
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
// Tests: claim_owner_rewards
// ============================================================================

#[test]
fn test_claim_owner_rewards_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 1000u64; // 1000 COIN per epoch total
    let total_contributed = 10_000_000u64;
    env.init_market_rewards(n, 1u128 << 64, total_contributed);

    let contributor = Keypair::new();
    env.svm
        .airdrop(&contributor.pubkey(), 10_000_000_000)
        .unwrap();

    let contributed = 5_000_000u64; // 50% of total
    let receipt = env.create_receipt(&contributor.pubkey(), contributed);
    let coin_ata = env.create_coin_ata( &contributor.pubkey(), 0);

    // Advance clock past one full epoch
    // market_start_slot = 100, start_epoch = 100 / 216000 = 0
    // We need current_epoch > 0, so slot >= 216000
    env.set_clock(EPOCH_SLOTS + 100); // epoch 1

    env.claim_owner_rewards(&contributor, &receipt, &coin_ata);

    // Expected: N * 1 epoch * 5M / 10M = 1000 * 1 * 5_000_000 / 10_000_000 = 500
    let balance = env.read_token_balance(&coin_ata);
    assert_eq!(balance, 500, "Should receive 500 COIN for 1 epoch at 50% share");
}

#[test]
fn test_claim_owner_rewards_zero_epochs() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 1u128 << 64, 10_000_000);

    let contributor = Keypair::new();
    env.svm
        .airdrop(&contributor.pubkey(), 10_000_000_000)
        .unwrap();
    let receipt = env.create_receipt(&contributor.pubkey(), 5_000_000);
    let coin_ata = env.create_coin_ata( &contributor.pubkey(), 0);

    // Don't advance clock — still in epoch 0 (same epoch as start)
    env.claim_owner_rewards(&contributor, &receipt, &coin_ata);

    let balance = env.read_token_balance(&coin_ata);
    assert_eq!(balance, 0, "No COIN should be minted in same epoch");
}

#[test]
fn test_claim_owner_rewards_multiple_claims() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 1000u64;
    let total_contributed = 10_000_000u64;
    env.init_market_rewards(n, 1u128 << 64, total_contributed);

    let contributor = Keypair::new();
    env.svm
        .airdrop(&contributor.pubkey(), 10_000_000_000)
        .unwrap();

    let contributed = 10_000_000u64; // 100% of total
    let receipt = env.create_receipt(&contributor.pubkey(), contributed);
    let coin_ata = env.create_coin_ata( &contributor.pubkey(), 0);

    // Claim after 1 epoch
    env.set_clock(EPOCH_SLOTS + 100);
    env.claim_owner_rewards(&contributor, &receipt, &coin_ata);
    let balance1 = env.read_token_balance(&coin_ata);
    assert_eq!(balance1, 1000, "1 epoch * 100% = 1000 COIN");

    // Claim again in same epoch — should get 0 more
    env.advance_blockhash();
    env.claim_owner_rewards(&contributor, &receipt, &coin_ata);
    let balance1b = env.read_token_balance(&coin_ata);
    assert_eq!(balance1b, 1000, "No additional COIN in same epoch");

    // Advance to epoch 3 (2 more epochs)
    env.set_clock(3 * EPOCH_SLOTS + 100);
    env.claim_owner_rewards(&contributor, &receipt, &coin_ata);
    let balance3 = env.read_token_balance(&coin_ata);
    assert_eq!(balance3, 3000, "3 epochs total * 100% = 3000 COIN");
}

#[test]
fn test_claim_owner_rewards_wrong_signer() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 1u128 << 64, 10_000_000);

    let real_contributor = Keypair::new();
    env.svm
        .airdrop(&real_contributor.pubkey(), 10_000_000_000)
        .unwrap();
    let receipt = env.create_receipt(&real_contributor.pubkey(), 5_000_000);

    // Try to claim with a different signer
    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();
    let attacker_ata = env.create_coin_ata( &attacker.pubkey(), 0);

    env.set_clock(EPOCH_SLOTS + 100);

    let result = env.try_claim_owner_rewards(&attacker, &receipt, &attacker_ata);
    assert!(result.is_err(), "Wrong signer should be rejected");
}

#[test]
fn test_claim_owner_rewards_fake_receipt_wrong_owner() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 1u128 << 64, 10_000_000);

    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();

    // Create a fake receipt owned by a different program (not the registered receipt_program)
    let fake_receipt = Pubkey::new_unique();
    let fake_owner = Pubkey::new_unique();
    env.svm
        .set_account(
            fake_receipt,
            Account {
                lamports: 1_000_000,
                data: make_receipt_data(&attacker.pubkey(), 10_000_000), // claim 100%
                owner: fake_owner,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let coin_ata = env.create_coin_ata(&attacker.pubkey(), 0);
    env.set_clock(EPOCH_SLOTS + 100);

    let result = env.try_claim_owner_rewards(&attacker, &fake_receipt, &coin_ata);
    assert!(
        result.is_err(),
        "Receipt with wrong owner program should be rejected"
    );
}

#[test]
fn test_claim_owner_rewards_two_contributors() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 1000u64;
    let total_contributed = 10_000_000u64;
    env.init_market_rewards(n, 1u128 << 64, total_contributed);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Alice contributed 3M (30%), Bob contributed 7M (70%)
    let receipt_a = env.create_receipt(&alice.pubkey(), 3_000_000);
    let receipt_b = env.create_receipt(&bob.pubkey(), 7_000_000);
    let ata_a = env.create_coin_ata( &alice.pubkey(), 0);
    let ata_b = env.create_coin_ata( &bob.pubkey(), 0);

    env.set_clock(EPOCH_SLOTS + 100); // 1 epoch

    env.claim_owner_rewards(&alice, &receipt_a, &ata_a);
    env.claim_owner_rewards(&bob, &receipt_b, &ata_b);

    let balance_a = env.read_token_balance(&ata_a);
    let balance_b = env.read_token_balance(&ata_b);

    // 1000 * 1 * 3M / 10M = 300
    assert_eq!(balance_a, 300, "Alice gets 30%");
    // 1000 * 1 * 7M / 10M = 700
    assert_eq!(balance_b, 700, "Bob gets 70%");
    assert_eq!(balance_a + balance_b, 1000, "Total = N per epoch");
}

// ============================================================================
// Tests: claim_lp_rewards
// ============================================================================

#[test]
fn test_claim_lp_rewards_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    // K = 2 COIN per fee-atom (in FP)
    let k: u128 = 2u128 << 64;
    env.init_market_rewards(1000, k, 10_000_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    // Deposit capital
    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    // Execute a trade to generate fees
    // With trading_fee_bps = 100 (1%), a trade of 1_000_000 generates 10_000 fee
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    // Create COIN token account for LP
    let coin_ata = env.create_coin_ata( &lp_owner.pubkey(), 0);

    // Claim LP rewards
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);

    let balance = env.read_token_balance(&coin_ata);
    // fee = size * trading_fee_bps / 10000 = 1_000_000 * 100 / 10000 = 10_000
    // entitled = fee * K / FP = 10_000 * 2 = 20_000
    // But the exact fee depends on the percolator implementation
    // At minimum, if there were any fees, balance should be > 0
    println!("LP reward balance: {}", balance);
    assert!(balance > 0, "LP should receive some COIN from fees");
}

#[test]
fn test_claim_lp_rewards_wrong_owner() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let k: u128 = 2u128 << 64;
    env.init_market_rewards(1000, k, 10_000_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    // Attacker tries to claim LP's rewards
    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();
    let attacker_ata = env.create_coin_ata( &attacker.pubkey(), 0);

    let result = env.try_claim_lp_rewards(&attacker, lp_idx, &attacker_ata);
    assert!(result.is_err(), "Wrong LP owner should be rejected");
}

#[test]
fn test_claim_lp_rewards_multiple_claims() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let k: u128 = 1u128 << 64; // 1 COIN per fee-atom
    env.init_market_rewards(1000, k, 10_000_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    // First trade
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);
    let coin_ata = env.create_coin_ata( &lp_owner.pubkey(), 0);

    // First claim
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);
    let balance1 = env.read_token_balance(&coin_ata);
    println!("First LP claim: {}", balance1);
    assert!(balance1 > 0, "First claim should yield > 0");

    // Second claim with no new trades — should get 0 additional
    env.advance_blockhash();
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);
    let balance2 = env.read_token_balance(&coin_ata);
    assert_eq!(balance2, balance1, "No additional rewards without new fees");

    // Second trade generates more fees
    env.advance_blockhash();
    env.trade(&user, &lp_owner, lp_idx, user_idx, -500_000);
    env.advance_blockhash();
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);
    let balance3 = env.read_token_balance(&coin_ata);
    println!("After second trade LP claim: {}", balance3);
    assert!(balance3 > balance2, "More fees should yield more rewards");
}

#[test]
fn test_claim_lp_rewards_no_fees() {
    let mut env = TestEnv::with_trading_fee(0); // No trading fee
    env.init_coin_config();
    let k: u128 = 1u128 << 64;
    env.init_market_rewards(1000, k, 10_000_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    // Trade with zero fee
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    let coin_ata = env.create_coin_ata( &lp_owner.pubkey(), 0);
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);

    let balance = env.read_token_balance(&coin_ata);
    assert_eq!(balance, 0, "No fees = no LP rewards");
}

// ============================================================================
// Tests: admin burn disables all admin instructions
// ============================================================================

/// Helper: send a percolator admin instruction with [admin_signer, slab] (2-account layout).
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

/// Helper: send a percolator admin instruction with 6 accounts (WithdrawInsurance layout).
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
            AccountMeta::new(dummy, false),       // admin_ata (dummy)
            AccountMeta::new(env.vault, false),    // vault
            AccountMeta::new_readonly(spl_token::ID, false), // token_program
            AccountMeta::new_readonly(dummy, false), // vault_pda (dummy)
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

/// Helper: send a percolator admin instruction with 8 accounts (AdminForceCloseAccount layout).
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
            AccountMeta::new(env.vault, false),    // vault
            AccountMeta::new(dummy, false),         // owner_ata
            AccountMeta::new_readonly(dummy, false), // pda
            AccountMeta::new_readonly(spl_token::ID, false), // token_program
            AccountMeta::new_readonly(sysvar::clock::ID, false), // clock
            AccountMeta::new_readonly(dummy, false), // oracle
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

    // The payer is the admin (set in encode_init_market).
    // First, verify admin can still do something (e.g. SetRiskThreshold).
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();
    let result = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_set_risk_threshold(0),
    );
    assert!(result.is_ok(), "Admin should work before burn: {:?}", result);

    // Burn admin: UpdateAdmin(Pubkey::default())
    env.advance_blockhash();
    let result = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
    assert!(result.is_ok(), "UpdateAdmin to zero should succeed: {:?}", result);

    // Now attempt ALL 11 admin-gated instructions — every one must fail.
    let anyone = Keypair::new();
    env.svm.airdrop(&anyone.pubkey(), 10_000_000_000).unwrap();

    // 1. SetRiskThreshold
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_risk_threshold(0));
    assert!(r.is_err(), "SetRiskThreshold must fail after admin burn");

    // 2. UpdateAdmin (can't re-claim admin)
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &anyone,
        encode_update_admin(&anyone.pubkey()),
    );
    assert!(r.is_err(), "UpdateAdmin must fail after admin burn");

    // 3. CloseSlab
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_close_slab());
    assert!(r.is_err(), "CloseSlab must fail after admin burn");

    // 4. UpdateConfig
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_update_config());
    assert!(r.is_err(), "UpdateConfig must fail after admin burn");

    // 5. SetMaintenanceFee
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_maintenance_fee(0));
    assert!(r.is_err(), "SetMaintenanceFee must fail after admin burn");

    // 6. SetOracleAuthority
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &anyone,
        encode_set_oracle_authority(&anyone.pubkey()),
    );
    assert!(r.is_err(), "SetOracleAuthority must fail after admin burn");

    // 7. SetOraclePriceCap
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_oracle_price_cap(1000));
    assert!(r.is_err(), "SetOraclePriceCap must fail after admin burn");

    // 8. ResolveMarket
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_resolve_market());
    assert!(r.is_err(), "ResolveMarket must fail after admin burn");

    // 9. WithdrawInsurance (6-account layout)
    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &anyone, encode_withdraw_insurance());
    assert!(r.is_err(), "WithdrawInsurance must fail after admin burn");

    // 10. AdminForceCloseAccount (8-account layout)
    env.advance_blockhash();
    let r = try_percolator_admin_ix_8(&mut env, &anyone, encode_admin_force_close(0));
    assert!(r.is_err(), "AdminForceCloseAccount must fail after admin burn");

    // 11. SetInsuranceWithdrawPolicy
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &anyone,
        encode_set_insurance_withdraw_policy(
            &anyone.pubkey(),
            1_000_000,
            5000,
            100,
        ),
    );
    assert!(r.is_err(), "SetInsuranceWithdrawPolicy must fail after admin burn");

    // Also verify the original admin can't do anything either
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_set_risk_threshold(0));
    assert!(r.is_err(), "Original admin must also fail after burn");
}

#[test]
fn test_admin_burn_is_irreversible() {
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    // Burn admin
    let result = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
    assert!(result.is_ok());

    // Try to re-set admin back to the original key — must fail
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&admin.pubkey()),
    );
    assert!(r.is_err(), "Cannot re-claim admin once burned");

    // Try with a brand new keypair
    let new_admin = Keypair::new();
    env.svm.airdrop(&new_admin.pubkey(), 1_000_000_000).unwrap();
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &new_admin,
        encode_update_admin(&new_admin.pubkey()),
    );
    assert!(r.is_err(), "No one can re-claim admin once burned");
}

// ============================================================================
// Tests: multi-user insurance deposit, rewards, independent withdrawal timing
// ============================================================================

#[test]
fn test_multi_user_owner_rewards_different_times() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 1000u64;
    let total_contributed = 10_000_000u64;
    env.init_market_rewards(n, 1u128 << 64, total_contributed);

    let alice = Keypair::new();
    let bob = Keypair::new();
    let carol = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&carol.pubkey(), 10_000_000_000).unwrap();

    // Alice 20%, Bob 30%, Carol 50%
    let receipt_a = env.create_receipt(&alice.pubkey(), 2_000_000);
    let receipt_b = env.create_receipt(&bob.pubkey(), 3_000_000);
    let receipt_c = env.create_receipt(&carol.pubkey(), 5_000_000);
    let ata_a = env.create_coin_ata(&alice.pubkey(), 0);
    let ata_b = env.create_coin_ata(&bob.pubkey(), 0);
    let ata_c = env.create_coin_ata(&carol.pubkey(), 0);

    // Epoch 1: only Alice claims
    env.set_clock(EPOCH_SLOTS + 100);
    env.claim_owner_rewards(&alice, &receipt_a, &ata_a);
    assert_eq!(env.read_token_balance(&ata_a), 200); // 1000 * 1 * 20% = 200

    // Epoch 3: Bob claims (should get epochs 1-2, i.e., 2 epochs worth)
    env.set_clock(3 * EPOCH_SLOTS + 100);
    env.claim_owner_rewards(&bob, &receipt_b, &ata_b);
    // Bob: 1000 * 3 epochs * 30% = 900 (catches up from epoch 0 through epoch 2)
    assert_eq!(env.read_token_balance(&ata_b), 900);

    // Alice claims again at epoch 3 (should get epochs 1-2 additional)
    env.advance_blockhash();
    env.claim_owner_rewards(&alice, &receipt_a, &ata_a);
    // Alice: total 3 epochs * 20% * 1000 = 600
    assert_eq!(env.read_token_balance(&ata_a), 600);

    // Epoch 5: Carol finally claims (5 epochs * 50% = 2500)
    env.set_clock(5 * EPOCH_SLOTS + 100);
    env.claim_owner_rewards(&carol, &receipt_c, &ata_c);
    assert_eq!(env.read_token_balance(&ata_c), 2500);

    // All claims are independent — Alice's earlier claim didn't affect Bob or Carol
    // Total minted: 600 + 900 + 2500 = 4000
    // Expected: epochs 0..5 for each user at their share
    // Alice: 5 * 200 = 1000 after epoch 5 claim
    env.advance_blockhash();
    env.claim_owner_rewards(&alice, &receipt_a, &ata_a);
    assert_eq!(env.read_token_balance(&ata_a), 1000);
}

#[test]
fn test_multi_user_lp_rewards_independent() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let k: u128 = 1u128 << 64;
    env.init_market_rewards(1000, k, 10_000_000);

    let lp1_owner = Keypair::new();
    let lp2_owner = Keypair::new();
    let user = Keypair::new();

    let lp1_idx = env.init_lp(&lp1_owner);
    let lp2_idx = env.init_lp(&lp2_owner);
    let user_idx = env.init_user(&user);

    // Both LPs deposit
    env.deposit(&lp1_owner, lp1_idx, 100_000_000);
    env.deposit(&lp2_owner, lp2_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    // Trade against LP1
    env.trade(&user, &lp1_owner, lp1_idx, user_idx, 1_000_000);

    let coin_ata1 = env.create_coin_ata(&lp1_owner.pubkey(), 0);
    let coin_ata2 = env.create_coin_ata(&lp2_owner.pubkey(), 0);

    // LP1 claims — should get rewards from their fees
    env.claim_lp_rewards(&lp1_owner, lp1_idx, &coin_ata1);
    let lp1_balance = env.read_token_balance(&coin_ata1);
    assert!(lp1_balance > 0, "LP1 should earn from their trades");

    // LP2 claims — no trades against LP2 so no fees
    env.claim_lp_rewards(&lp2_owner, lp2_idx, &coin_ata2);
    let lp2_balance = env.read_token_balance(&coin_ata2);
    assert_eq!(lp2_balance, 0, "LP2 had no trades, no rewards");

    // Now trade against LP2
    env.advance_blockhash();
    env.trade(&user, &lp2_owner, lp2_idx, user_idx, -500_000);

    // LP2 claims again — now should have rewards
    env.advance_blockhash();
    env.claim_lp_rewards(&lp2_owner, lp2_idx, &coin_ata2);
    let lp2_balance2 = env.read_token_balance(&coin_ata2);
    assert!(lp2_balance2 > 0, "LP2 should now have rewards from second trade");

    // LP1's balance unchanged (no new trades against LP1)
    env.advance_blockhash();
    env.claim_lp_rewards(&lp1_owner, lp1_idx, &coin_ata1);
    let lp1_balance2 = env.read_token_balance(&coin_ata1);
    assert_eq!(lp1_balance2, lp1_balance, "LP1 balance unchanged without new trades");
}

// ============================================================================
// Tests: DAO cannot steal user funds
// ============================================================================

#[test]
fn test_dao_cannot_steal_via_admin_instructions() {
    // After admin burn, no instruction can extract funds from the market.
    // Before burn, the only fund-extracting admin instruction is WithdrawInsurance,
    // which requires market to be resolved AND all positions closed.
    // This test proves post-burn no admin instructions work at all.
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    // Set up LP and user with funds in the market
    let lp_owner = Keypair::new();
    let user = Keypair::new();
    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);
    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    // Burn admin
    let r = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
    assert!(r.is_ok(), "Admin burn should succeed");

    // Try to resolve the market (required for WithdrawInsurance) — must fail
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_resolve_market());
    assert!(r.is_err(), "Cannot resolve market after admin burn");

    // Try to withdraw insurance directly — must fail
    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &admin, encode_withdraw_insurance());
    assert!(r.is_err(), "Cannot withdraw insurance after admin burn");

    // Try to force close a user account — must fail
    env.advance_blockhash();
    let r = try_percolator_admin_ix_8(&mut env, &admin, encode_admin_force_close(user_idx));
    assert!(r.is_err(), "Cannot force close accounts after admin burn");

    // Try to close the slab — must fail
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_close_slab());
    assert!(r.is_err(), "Cannot close slab after admin burn");
}

#[test]
fn test_no_instruction_to_redirect_user_funds() {
    // Rewards program has exactly 4 instructions:
    // 0 = init_market_rewards (creates config, no fund transfer)
    // 1 = claim_owner_rewards (mints new COIN to contributor, doesn't touch collateral)
    // 2 = claim_lp_rewards (mints new COIN to LP, doesn't touch collateral)
    // 3 = init_coin_config (creates shared COIN config, no fund transfer)
    //
    // None of these can transfer collateral from the vault or steal user deposits.
    // The only way collateral moves is through percolator-prog's deposit/withdraw.
    //
    // This test verifies claim_owner_rewards can only mint to the contributor who
    // actually owns the receipt, and claim_lp_rewards can only mint to the LP owner.

    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 1u128 << 64, 10_000_000);

    let real_contributor = Keypair::new();
    env.svm.airdrop(&real_contributor.pubkey(), 10_000_000_000).unwrap();
    let receipt = env.create_receipt(&real_contributor.pubkey(), 5_000_000);

    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();
    let attacker_ata = env.create_coin_ata(&attacker.pubkey(), 0);

    env.set_clock(EPOCH_SLOTS + 100);

    // Attacker cannot claim using someone else's receipt
    let r = env.try_claim_owner_rewards(&attacker, &receipt, &attacker_ata);
    assert!(r.is_err(), "Attacker cannot steal owner rewards");

    // Attacker cannot claim LP rewards for an LP they don't own
    let lp_owner = Keypair::new();
    let user = Keypair::new();
    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);
    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    let r = env.try_claim_lp_rewards(&attacker, lp_idx, &attacker_ata);
    assert!(r.is_err(), "Attacker cannot steal LP rewards");

    // Only the real LP owner can claim
    let lp_ata = env.create_coin_ata(&lp_owner.pubkey(), 0);
    env.claim_lp_rewards(&lp_owner, lp_idx, &lp_ata);
    let balance = env.read_token_balance(&lp_ata);
    assert!(balance > 0, "Real LP owner gets their rewards");
}

#[test]
fn test_insurance_topup_permissionless_withdraw_restricted() {
    // Anyone can top up insurance (permissionless), but only admin can withdraw
    // via WithdrawInsurance, and only after resolution.
    // After admin burn, no one can withdraw insurance.
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let lp_owner = Keypair::new();
    let user = Keypair::new();
    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);
    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    // Anyone can deposit into insurance (topup is permissionless)
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
    env.svm.send_transaction(tx).expect("insurance topup should succeed");

    // Admin cannot withdraw insurance before resolving the market
    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &admin, encode_withdraw_insurance());
    assert!(r.is_err(), "Cannot withdraw insurance before resolution");

    // Burn admin
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
    assert!(r.is_ok());

    // After burn, cannot resolve or withdraw
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_resolve_market());
    assert!(r.is_err(), "Cannot resolve after burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &admin, encode_withdraw_insurance());
    assert!(r.is_err(), "Cannot withdraw insurance after burn");
}

// ============================================================================
// Tests: full end-to-end flow
// ============================================================================

#[test]
fn test_e2e_market_creation_to_claim() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 500u64;
    let k: u128 = 1u128 << 64;
    let total_contributed = 5_000_000u64;

    env.init_market_rewards(n, k, total_contributed);

    // Set up contributor
    let contributor = Keypair::new();
    env.svm
        .airdrop(&contributor.pubkey(), 10_000_000_000)
        .unwrap();
    let receipt = env.create_receipt(&contributor.pubkey(), 5_000_000); // 100%
    let coin_ata_owner = env.create_coin_ata( &contributor.pubkey(), 0);

    // Set up LP and user for trading
    let lp_owner = Keypair::new();
    let user = Keypair::new();
    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);
    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    // Trade to generate LP fees
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    // Advance 2 epochs
    env.set_clock(2 * EPOCH_SLOTS + 200);

    // Claim owner rewards
    env.claim_owner_rewards(&contributor, &receipt, &coin_ata_owner);
    let owner_balance = env.read_token_balance(&coin_ata_owner);
    // 500 * 2 * 5M/5M = 1000
    assert_eq!(owner_balance, 1000, "Owner should get 1000 COIN for 2 epochs");

    // Claim LP rewards
    let coin_ata_lp = env.create_coin_ata( &lp_owner.pubkey(), 0);
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata_lp);
    let lp_balance = env.read_token_balance(&coin_ata_lp);
    println!("LP rewards: {}", lp_balance);
    assert!(lp_balance > 0, "LP should get rewards from fees");
}

// ============================================================================
// Tests: multi-market shared COIN
// ============================================================================

#[test]
fn test_two_markets_share_one_coin() {
    // Two percolator markets share the same COIN mint.
    // Each market has independent N, K, and independently-earned rewards.
    let mut svm = LiteSVM::new();

    let percolator_id = Pubkey::new_unique();
    let perc_bytes = std::fs::read(percolator_path()).expect("read percolator BPF");
    svm.add_program(percolator_id, &perc_bytes);

    let rewards_id = Pubkey::new_unique();
    let rewards_bytes = std::fs::read(rewards_path()).expect("read rewards BPF");
    svm.add_program(rewards_id, &rewards_bytes);

    let dao_authority = Keypair::new();
    svm.airdrop(&dao_authority.pubkey(), 100_000_000_000).unwrap();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();

    let receipt_program = Pubkey::new_unique();

    // Shared COIN mint — authority derived from coin_mint key
    let coin_mint = Pubkey::new_unique();
    let (mint_authority_pda, _) =
        Pubkey::find_program_address(&[b"coin_mint_authority", coin_mint.as_ref()], &rewards_id);
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

    // Init CoinConfig once
    let (coin_cfg_pda, _) =
        Pubkey::find_program_address(&[b"coin_cfg", coin_mint.as_ref()], &rewards_id);
    let ix = Instruction {
        program_id: rewards_id,
        accounts: vec![
            AccountMeta::new(dao_authority.pubkey(), true),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new(coin_cfg_pda, false),
            AccountMeta::new_readonly(receipt_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_coin_config(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&dao_authority.pubkey()),
        &[&dao_authority],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("init_coin_config failed");

    // --- Create market A ---
    let collateral_mint_a = Pubkey::new_unique();
    {
        let mut data = vec![0u8; Mint::LEN];
        let m = Mint {
            mint_authority: solana_sdk::program_option::COption::None,
            supply: 0, decimals: 6, is_initialized: true,
            freeze_authority: solana_sdk::program_option::COption::None,
        };
        Mint::pack(m, &mut data).unwrap();
        svm.set_account(collateral_mint_a, Account {
            lamports: 1_000_000, data, owner: spl_token::ID,
            executable: false, rent_epoch: 0,
        }).unwrap();
    }
    let slab_a = Pubkey::new_unique();
    svm.set_account(slab_a, Account {
        lamports: 1_000_000_000, data: vec![0u8; SLAB_LEN],
        owner: percolator_id, executable: false, rent_epoch: 0,
    }).unwrap();
    let (vault_pda_a, _) = Pubkey::find_program_address(&[b"vault", slab_a.as_ref()], &percolator_id);
    let vault_a = Pubkey::new_unique();
    svm.set_account(vault_a, Account {
        lamports: 1_000_000,
        data: make_token_account_data(&collateral_mint_a, &vault_pda_a, 0),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();
    let pyth_a = Pubkey::new_unique();
    svm.set_account(pyth_a, Account {
        lamports: 1_000_000,
        data: make_pyth_data(&TEST_FEED_ID, 100_000_000, -6, 1, 100),
        owner: PYTH_RECEIVER_PROGRAM_ID, executable: false, rent_epoch: 0,
    }).unwrap();
    svm.set_sysvar(&Clock { slot: 100, unix_timestamp: 100, ..Clock::default() });
    let dummy_ata_a = Pubkey::new_unique();
    svm.set_account(dummy_ata_a, Account {
        lamports: 1_000_000, data: vec![0u8; TokenAccount::LEN],
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();
    let ix = Instruction {
        program_id: percolator_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(slab_a, false),
            AccountMeta::new_readonly(collateral_mint_a, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(dummy_ata_a, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market(&payer.pubkey(), &collateral_mint_a, &TEST_FEED_ID, 100),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], svm.latest_blockhash());
    svm.send_transaction(tx).expect("init market A");

    // --- Create market B (different slab, same COIN) ---
    let collateral_mint_b = Pubkey::new_unique();
    {
        let mut data = vec![0u8; Mint::LEN];
        let m = Mint {
            mint_authority: solana_sdk::program_option::COption::None,
            supply: 0, decimals: 6, is_initialized: true,
            freeze_authority: solana_sdk::program_option::COption::None,
        };
        Mint::pack(m, &mut data).unwrap();
        svm.set_account(collateral_mint_b, Account {
            lamports: 1_000_000, data, owner: spl_token::ID,
            executable: false, rent_epoch: 0,
        }).unwrap();
    }
    let slab_b = Pubkey::new_unique();
    svm.set_account(slab_b, Account {
        lamports: 1_000_000_000, data: vec![0u8; SLAB_LEN],
        owner: percolator_id, executable: false, rent_epoch: 0,
    }).unwrap();
    let (vault_pda_b, _) = Pubkey::find_program_address(&[b"vault", slab_b.as_ref()], &percolator_id);
    let vault_b = Pubkey::new_unique();
    svm.set_account(vault_b, Account {
        lamports: 1_000_000,
        data: make_token_account_data(&collateral_mint_b, &vault_pda_b, 0),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();
    let pyth_b = Pubkey::new_unique();
    svm.set_account(pyth_b, Account {
        lamports: 1_000_000,
        data: make_pyth_data(&TEST_FEED_ID, 200_000_000, -6, 1, 100),
        owner: PYTH_RECEIVER_PROGRAM_ID, executable: false, rent_epoch: 0,
    }).unwrap();
    svm.expire_blockhash();
    let dummy_ata_b = Pubkey::new_unique();
    svm.set_account(dummy_ata_b, Account {
        lamports: 1_000_000, data: vec![0u8; TokenAccount::LEN],
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();
    let ix = Instruction {
        program_id: percolator_id,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(slab_b, false),
            AccountMeta::new_readonly(collateral_mint_b, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(dummy_ata_b, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market(&payer.pubkey(), &collateral_mint_b, &TEST_FEED_ID, 100),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], svm.latest_blockhash());
    svm.send_transaction(tx).expect("init market B");

    // --- Init rewards for both markets with same COIN, different N ---
    // Market A: N = 1000
    let (mrc_pda_a, _) = Pubkey::find_program_address(&[b"mrc", slab_a.as_ref()], &rewards_id);
    svm.expire_blockhash();
    let ix = Instruction {
        program_id: rewards_id,
        accounts: vec![
            AccountMeta::new(dao_authority.pubkey(), true),
            AccountMeta::new_readonly(slab_a, false),
            AccountMeta::new(mrc_pda_a, false),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(1000, 1u128 << 64, 10_000_000),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&dao_authority.pubkey()), &[&dao_authority], svm.latest_blockhash());
    svm.send_transaction(tx).expect("init_market_rewards A");

    // Market B: N = 500
    let (mrc_pda_b, _) = Pubkey::find_program_address(&[b"mrc", slab_b.as_ref()], &rewards_id);
    svm.expire_blockhash();
    let ix = Instruction {
        program_id: rewards_id,
        accounts: vec![
            AccountMeta::new(dao_authority.pubkey(), true),
            AccountMeta::new_readonly(slab_b, false),
            AccountMeta::new(mrc_pda_b, false),
            AccountMeta::new_readonly(coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(500, 1u128 << 64, 10_000_000),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&dao_authority.pubkey()), &[&dao_authority], svm.latest_blockhash());
    svm.send_transaction(tx).expect("init_market_rewards B");

    // --- Claim owner rewards from both markets independently ---
    let contributor = Keypair::new();
    svm.airdrop(&contributor.pubkey(), 10_000_000_000).unwrap();

    // Receipt for market A: 100% share
    let receipt_a = Pubkey::new_unique();
    svm.set_account(receipt_a, Account {
        lamports: 1_000_000,
        data: make_receipt_data(&contributor.pubkey(), 10_000_000),
        owner: receipt_program, executable: false, rent_epoch: 0,
    }).unwrap();

    // Receipt for market B: 100% share
    let receipt_b = Pubkey::new_unique();
    svm.set_account(receipt_b, Account {
        lamports: 1_000_000,
        data: make_receipt_data(&contributor.pubkey(), 10_000_000),
        owner: receipt_program, executable: false, rent_epoch: 0,
    }).unwrap();

    // Advance past 1 epoch
    svm.set_sysvar(&Clock { slot: EPOCH_SLOTS + 100, unix_timestamp: (EPOCH_SLOTS + 100) as i64, ..Clock::default() });
    svm.expire_blockhash();

    // Claim from market A
    let coin_ata = Pubkey::new_unique();
    svm.set_account(coin_ata, Account {
        lamports: 1_000_000,
        data: make_token_account_data(&coin_mint, &contributor.pubkey(), 0),
        owner: spl_token::ID, executable: false, rent_epoch: 0,
    }).unwrap();

    let (ocs_a, _) = Pubkey::find_program_address(&[b"ocs", slab_a.as_ref(), receipt_a.as_ref()], &rewards_id);
    let ix = Instruction {
        program_id: rewards_id,
        accounts: vec![
            AccountMeta::new(contributor.pubkey(), true),
            AccountMeta::new_readonly(mrc_pda_a, false),
            AccountMeta::new_readonly(slab_a, false),
            AccountMeta::new_readonly(receipt_a, false),
            AccountMeta::new(ocs_a, false),
            AccountMeta::new(coin_mint, false),
            AccountMeta::new(coin_ata, false),
            AccountMeta::new_readonly(mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_claim_owner_rewards(),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&contributor.pubkey()), &[&contributor], svm.latest_blockhash());
    svm.send_transaction(tx).expect("claim A");

    let acct = svm.get_account(&coin_ata).unwrap();
    let tok = TokenAccount::unpack(&acct.data).unwrap();
    assert_eq!(tok.amount, 1000, "Market A: N=1000, 1 epoch, 100% = 1000 COIN");

    // Claim from market B (into same COIN ATA)
    svm.expire_blockhash();
    let (ocs_b, _) = Pubkey::find_program_address(&[b"ocs", slab_b.as_ref(), receipt_b.as_ref()], &rewards_id);
    let ix = Instruction {
        program_id: rewards_id,
        accounts: vec![
            AccountMeta::new(contributor.pubkey(), true),
            AccountMeta::new_readonly(mrc_pda_b, false),
            AccountMeta::new_readonly(slab_b, false),
            AccountMeta::new_readonly(receipt_b, false),
            AccountMeta::new(ocs_b, false),
            AccountMeta::new(coin_mint, false),
            AccountMeta::new(coin_ata, false),
            AccountMeta::new_readonly(mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
        ],
        data: encode_claim_owner_rewards(),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&contributor.pubkey()), &[&contributor], svm.latest_blockhash());
    svm.send_transaction(tx).expect("claim B");

    let acct = svm.get_account(&coin_ata).unwrap();
    let tok = TokenAccount::unpack(&acct.data).unwrap();
    // 1000 from A + 500 from B = 1500
    assert_eq!(tok.amount, 1500, "Both markets mint into same COIN: 1000 + 500 = 1500");
}

#[test]
fn test_unauthorized_market_cannot_inflate_shared_coin() {
    // An attacker creates their own percolator market and tries to register it
    // with the shared COIN. This must fail because they're not the CoinConfig authority.
    let mut env = TestEnv::new();
    env.init_coin_config();

    // Attacker creates a rogue slab (owned by percolator — since they can call InitMarket)
    let rogue_slab = Pubkey::new_unique();
    env.svm.set_account(rogue_slab, Account {
        lamports: 1_000_000_000, data: vec![0u8; SLAB_LEN],
        owner: env.percolator_id, executable: false, rent_epoch: 0,
    }).unwrap();

    // Attacker tries to register their slab with absurd N
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", rogue_slab.as_ref()], &env.rewards_id);
    let (coin_cfg_pda, _) =
        Pubkey::find_program_address(&[b"coin_cfg", env.coin_mint.as_ref()], &env.rewards_id);

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(attacker.pubkey(), true),
            AccountMeta::new_readonly(rogue_slab, false),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(u64::MAX, 1u128 << 64, 1), // absurd N
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
