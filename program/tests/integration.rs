//! Integration tests for the rewards program (staking vault design).
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

fn encode_init_market_rewards(n: u64, k: u128, epoch_slots: u64) -> Vec<u8> {
    let mut data = vec![0u8]; // tag = IX_INIT_MARKET_REWARDS
    data.extend_from_slice(&n.to_le_bytes());
    data.extend_from_slice(&k.to_le_bytes());
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

fn encode_claim_lp_rewards(lp_idx: u16) -> Vec<u8> {
    let mut data = vec![5u8]; // tag = IX_CLAIM_LP_REWARDS
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
    account_count: u16,
}

impl TestEnv {
    fn new() -> Self {
        Self::with_trading_fee(100) // 1% default trading fee for LP reward tests
    }

    fn with_trading_fee(trading_fee_bps: u64) -> Self {
        let mut svm = LiteSVM::new();

        let percolator_id = Pubkey::new_unique();
        let perc_bytes = std::fs::read(percolator_path()).expect("read percolator BPF");
        svm.add_program(percolator_id, &perc_bytes);

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
            account_count: 0,
        }
    }

    fn init_coin_config(&mut self) {
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", self.coin_mint.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
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

    fn try_init_coin_config_with_mint(&mut self, coin_mint: &Pubkey) -> Result<(), String> {
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", coin_mint.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new_readonly(*coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
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
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn init_market_rewards(&mut self, n: u64, k: u128, epoch_slots: u64) {
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

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true), // [0] authority
                AccountMeta::new_readonly(self.slab, false),        // [1] market_slab
                AccountMeta::new(mrc_pda, false),                   // [2] mrc PDA
                AccountMeta::new_readonly(self.coin_mint, false),   // [3] coin_mint
                AccountMeta::new_readonly(coin_cfg_pda, false),     // [4] coin_config
                AccountMeta::new_readonly(self.collateral_mint, false), // [5] collateral_mint
                AccountMeta::new(stake_vault, false),               // [6] stake_vault
                AccountMeta::new_readonly(spl_token::ID, false),    // [7] token_program
                AccountMeta::new_readonly(sysvar::rent::ID, false), // [8] rent
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // [9] system
            ],
            data: encode_init_market_rewards(n, k, epoch_slots),
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
        epoch_slots: u64,
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

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(self.collateral_mint, false),
                AccountMeta::new(stake_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market_rewards(n, k, epoch_slots),
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

    fn stake(&mut self, user: &Keypair, amount: u64) {
        let result = self.try_stake(user, amount);
        result.expect("stake failed");
    }

    fn try_stake(&mut self, user: &Keypair, amount: u64) -> Result<(), String> {
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
        let user_ata = self.create_ata(&col_mint, &user.pubkey(), amount);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),           // [0] user
                AccountMeta::new(mrc_pda, false),                // [1] mrc
                AccountMeta::new_readonly(self.slab, false),     // [2] market_slab
                AccountMeta::new(user_ata, false),               // [3] user_collateral_ata
                AccountMeta::new(stake_vault, false),            // [4] stake_vault
                AccountMeta::new(sp_pda, false),                 // [5] stake_position
                AccountMeta::new_readonly(spl_token::ID, false), // [6] token_program
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
                AccountMeta::new(user.pubkey(), true),               // [0] user
                AccountMeta::new(mrc_pda, false),                    // [1] mrc
                AccountMeta::new_readonly(self.slab, false),         // [2] market_slab
                AccountMeta::new(user_ata, false),                   // [3] user_collateral_ata
                AccountMeta::new(stake_vault, false),                // [4] stake_vault
                AccountMeta::new(sp_pda, false),                     // [5] stake_position
                AccountMeta::new(self.coin_mint, false),             // [6] coin_mint
                AccountMeta::new(coin_ata, false),                   // [7] user_coin_ata
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

    let (coin_cfg_pda, _) =
        Pubkey::find_program_address(&[b"coin_cfg", env.coin_mint.as_ref()], &env.rewards_id);
    let cfg_account = env.svm.get_account(&coin_cfg_pda).unwrap();
    assert_eq!(cfg_account.owner, env.rewards_id);
    assert_eq!(cfg_account.data.len(), 40); // COIN_CFG_SIZE = 8 + 32

    assert_eq!(&cfg_account.data[..8], b"CCFG_INI");
    let stored_auth = Pubkey::new_from_array(cfg_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_auth, env.dao_authority.pubkey());
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
    let k = 1u128 << 64;
    let epoch_slots = 216_000u64;

    env.init_market_rewards(n, k, epoch_slots);

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", env.slab.as_ref()], &env.rewards_id);
    let mrc_account = env.svm.get_account(&mrc_pda).unwrap();
    assert_eq!(mrc_account.owner, env.rewards_id);
    assert_eq!(mrc_account.data.len(), 176); // MRC_SIZE

    assert_eq!(&mrc_account.data[..8], b"MRC_V002");

    let stored_slab = Pubkey::new_from_array(mrc_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_slab, env.slab);

    let stored_mint = Pubkey::new_from_array(mrc_account.data[40..72].try_into().unwrap());
    assert_eq!(stored_mint, env.coin_mint);

    let stored_collateral =
        Pubkey::new_from_array(mrc_account.data[72..104].try_into().unwrap());
    assert_eq!(stored_collateral, env.collateral_mint);

    let stored_n = u64::from_le_bytes(mrc_account.data[104..112].try_into().unwrap());
    assert_eq!(stored_n, n);

    let stored_k = u128::from_le_bytes(mrc_account.data[112..128].try_into().unwrap());
    assert_eq!(stored_k, k);

    let stored_epoch_slots = u64::from_le_bytes(mrc_account.data[128..136].try_into().unwrap());
    assert_eq!(stored_epoch_slots, epoch_slots);

    let stored_start = u64::from_le_bytes(mrc_account.data[136..144].try_into().unwrap());
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
fn test_init_market_rewards_double_init_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 1u128 << 64, 216_000);

    env.advance_blockhash();
    let result = env.try_init_market_rewards(1000, 1u128 << 64, 216_000);
    assert!(result.is_err(), "Double init should fail");
}

#[test]
fn test_init_market_rewards_k_exceeds_max_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let max_k: u128 = 1_000_000u128 << 64;
    let result = env.try_init_market_rewards(1000, max_k + 1, 216_000);
    assert!(result.is_err(), "K > MAX_LP_COIN_PER_FEE_FP should fail");
}

#[test]
fn test_init_market_rewards_k_at_max_succeeds() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let max_k: u128 = 1_000_000u128 << 64;
    env.init_market_rewards(1000, max_k, 216_000);
}

#[test]
fn test_init_market_rewards_epoch_slots_zero_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let result = env.try_init_market_rewards(1000, 1u128 << 64, 0);
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
        data: encode_init_market_rewards(1000, 1u128 << 64, 216_000),
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
    env.init_market_rewards(1000, 0, 100); // N=1000, K=0, epoch_slots=100

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
    let total_staked = u64::from_le_bytes(mrc_data.data[168..176].try_into().unwrap());
    assert_eq!(total_staked, 1_000_000);
}

#[test]
fn test_stake_zero_amount_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    let result = env.try_stake(&user, 0);
    assert!(result.is_err(), "Staking 0 should fail");
}

#[test]
fn test_stake_additional_resets_lockup() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // First stake at slot 100
    env.stake(&user, 500_000);

    // Advance to slot 150 (within lockup)
    env.set_clock(150);
    // Stake more — lockup resets to slot 150
    env.stake(&user, 300_000);

    // Verify total amount
    let (sp_pda, _) = Pubkey::find_program_address(
        &[b"sp", env.slab.as_ref(), user.pubkey().as_ref()],
        &env.rewards_id,
    );
    let sp_data = env.svm.get_account(&sp_pda).unwrap();
    let amount = u64::from_le_bytes(sp_data.data[8..16].try_into().unwrap());
    assert_eq!(amount, 800_000);
    let deposit_slot = u64::from_le_bytes(sp_data.data[16..24].try_into().unwrap());
    assert_eq!(deposit_slot, 150); // lockup was reset
}

// ============================================================================
// Tests: unstake
// ============================================================================

#[test]
fn test_unstake_after_lockup() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let epoch_slots = 100u64;
    env.init_market_rewards(1000, 0, epoch_slots); // N=1000/epoch

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // Stake at slot 100
    env.stake(&user, 1_000_000);

    // Advance past lockup: slot >= 100 + 100 = 200
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
fn test_unstake_before_lockup_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    // Stake at slot 100
    env.stake(&user, 1_000_000);

    // Try to unstake at slot 150 (lockup not met: 100 + 100 = 200)
    env.set_clock(150);
    let result = env.try_unstake(&user, 1_000_000);
    assert!(result.is_err(), "Unstake before lockup should fail");
}

#[test]
fn test_unstake_more_than_staked_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

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
    env.init_market_rewards(1000, 0, epoch_slots);

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
fn test_full_unstake_closes_position() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

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
// Tests: claim_stake_rewards (without unstaking)
// ============================================================================

#[test]
fn test_claim_stake_rewards_no_lockup_required() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);

    // Advance half an epoch — within lockup but claim should still work
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
    env.init_market_rewards(1000, 0, 100);

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
fn test_claim_stake_rewards_zero_at_start() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

    let user = Keypair::new();
    env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();

    env.stake(&user, 1_000_000);

    // Claim immediately (same slot as stake) — should get 0
    let coin_ata = env.claim_stake_rewards(&user);
    assert_eq!(env.read_token_balance(&coin_ata), 0);
}

// ============================================================================
// Tests: multi-user staking accumulator
// ============================================================================

#[test]
fn test_two_users_equal_stake() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100); // N=1000/epoch, epoch_slots=100

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

    // Each gets half: 1000 * 1 / 2 = ~500 (rounding)
    assert!(bal_a >= 499 && bal_a <= 500, "Alice gets ~50%, got {}", bal_a);
    assert!(bal_b >= 499 && bal_b <= 500, "Bob gets ~50%, got {}", bal_b);
}

#[test]
fn test_two_users_different_amounts() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

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
}

#[test]
fn test_staker_joins_later() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

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
}

#[test]
fn test_staker_leaves_then_another_earns_all() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 0, 100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    // Both stake at slot 100
    env.stake(&alice, 1_000_000);
    env.advance_blockhash();
    env.stake(&bob, 1_000_000);

    // Advance past lockup
    env.set_clock(200);

    // Alice unstakes (full)
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
// Tests: claim_lp_rewards
// ============================================================================

#[test]
fn test_claim_lp_rewards_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let k: u128 = 2u128 << 64;
    env.init_market_rewards(1000, k, 216_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    let coin_ata = env.create_coin_ata(&lp_owner.pubkey(), 0);
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);

    let balance = env.read_token_balance(&coin_ata);
    println!("LP reward balance: {}", balance);
    assert!(balance > 0, "LP should receive some COIN from fees");
}

#[test]
fn test_claim_lp_rewards_wrong_owner() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let k: u128 = 2u128 << 64;
    env.init_market_rewards(1000, k, 216_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();
    let attacker_ata = env.create_coin_ata(&attacker.pubkey(), 0);

    let result = env.try_claim_lp_rewards(&attacker, lp_idx, &attacker_ata);
    assert!(result.is_err(), "Wrong LP owner should be rejected");
}

#[test]
fn test_claim_lp_rewards_multiple_claims() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let k: u128 = 1u128 << 64;
    env.init_market_rewards(1000, k, 216_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);
    let coin_ata = env.create_coin_ata(&lp_owner.pubkey(), 0);

    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);
    let balance1 = env.read_token_balance(&coin_ata);
    assert!(balance1 > 0);

    env.advance_blockhash();
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);
    let balance2 = env.read_token_balance(&coin_ata);
    assert_eq!(balance2, balance1, "No additional rewards without new fees");

    env.advance_blockhash();
    env.trade(&user, &lp_owner, lp_idx, user_idx, -500_000);
    env.advance_blockhash();
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);
    let balance3 = env.read_token_balance(&coin_ata);
    assert!(balance3 > balance2, "More fees should yield more rewards");
}

#[test]
fn test_claim_lp_rewards_no_fees() {
    let mut env = TestEnv::with_trading_fee(0);
    env.init_coin_config();
    let k: u128 = 1u128 << 64;
    env.init_market_rewards(1000, k, 216_000);

    let lp_owner = Keypair::new();
    let user = Keypair::new();

    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);

    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    let coin_ata = env.create_coin_ata(&lp_owner.pubkey(), 0);
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata);

    let balance = env.read_token_balance(&coin_ata);
    assert_eq!(balance, 0, "No fees = no LP rewards");
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
    let result = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_set_risk_threshold(0),
    );
    assert!(result.is_ok(), "Admin should work before burn: {:?}", result);

    env.advance_blockhash();
    let result = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
    assert!(result.is_ok(), "UpdateAdmin to zero should succeed: {:?}", result);

    let anyone = Keypair::new();
    env.svm.airdrop(&anyone.pubkey(), 10_000_000_000).unwrap();

    // All 11 admin instructions must fail after burn
    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_risk_threshold(0));
    assert!(r.is_err(), "SetRiskThreshold must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_update_admin(&anyone.pubkey()));
    assert!(r.is_err(), "UpdateAdmin must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_close_slab());
    assert!(r.is_err(), "CloseSlab must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_update_config());
    assert!(r.is_err(), "UpdateConfig must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_maintenance_fee(0));
    assert!(r.is_err(), "SetMaintenanceFee must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_oracle_authority(&anyone.pubkey()));
    assert!(r.is_err(), "SetOracleAuthority must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_set_oracle_price_cap(1000));
    assert!(r.is_err(), "SetOraclePriceCap must fail after admin burn");

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
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_set_risk_threshold(0));
    assert!(r.is_err(), "Original admin must also fail after burn");
}

#[test]
fn test_admin_burn_is_irreversible() {
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let result = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
    assert!(result.is_ok());

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_update_admin(&admin.pubkey()));
    assert!(r.is_err(), "Cannot re-claim admin once burned");

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
// Tests: DAO cannot steal user funds
// ============================================================================

#[test]
fn test_dao_cannot_steal_via_admin_instructions() {
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let lp_owner = Keypair::new();
    let user = Keypair::new();
    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);
    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);

    let r = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
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
    // Rewards program has exactly 6 instructions:
    // 0 = init_market_rewards (creates config + vault, no fund theft)
    // 1 = stake (user deposits collateral to vault, creating position)
    // 2 = unstake (user withdraws own collateral + claims COIN)
    // 3 = init_coin_config (creates shared COIN config, no fund transfer)
    // 4 = claim_stake_rewards (mints COIN to staker, no collateral transfer)
    // 5 = claim_lp_rewards (mints COIN to LP owner, no collateral transfer)
    //
    // Staked collateral can only be withdrawn by the staker who deposited it.
    // COIN is only minted, never transferred from user accounts.

    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 1u128 << 64, 100);

    let staker = Keypair::new();
    env.svm
        .airdrop(&staker.pubkey(), 10_000_000_000)
        .unwrap();
    env.stake(&staker, 1_000_000);

    // Attacker cannot claim staker's rewards
    let attacker = Keypair::new();
    env.svm
        .airdrop(&attacker.pubkey(), 10_000_000_000)
        .unwrap();
    let attacker_coin = env.create_coin_ata(&attacker.pubkey(), 0);

    env.set_clock(200);
    let result = env.try_claim_stake_rewards_to(&attacker, &attacker_coin);
    // This should fail because attacker has no StakePosition PDA
    assert!(result.is_err(), "Attacker cannot steal stake rewards");

    // Attacker cannot unstake staker's collateral
    let result = env.try_unstake(&attacker, 1_000_000);
    assert!(result.is_err(), "Attacker cannot unstake others' collateral");

    // Attacker cannot claim LP rewards for an LP they don't own
    let lp_owner = Keypair::new();
    let user = Keypair::new();
    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);
    env.deposit(&lp_owner, lp_idx, 100_000_000);
    env.deposit(&user, user_idx, 100_000_000);
    env.trade(&user, &lp_owner, lp_idx, user_idx, 1_000_000);

    let r = env.try_claim_lp_rewards(&attacker, lp_idx, &attacker_coin);
    assert!(r.is_err(), "Attacker cannot steal LP rewards");

    // Only the real LP owner can claim
    let lp_ata = env.create_coin_ata(&lp_owner.pubkey(), 0);
    env.claim_lp_rewards(&lp_owner, lp_idx, &lp_ata);
    assert!(env.read_token_balance(&lp_ata) > 0, "Real LP owner gets their rewards");
}

#[test]
fn test_insurance_topup_permissionless_withdraw_restricted() {
    let mut env = TestEnv::new();
    let admin = Keypair::from_bytes(&env.payer.to_bytes()).unwrap();

    let lp_owner = Keypair::new();
    let user = Keypair::new();
    let lp_idx = env.init_lp(&lp_owner);
    let user_idx = env.init_user(&user);
    env.deposit(&lp_owner, lp_idx, 100_000_000);
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
    let r = try_percolator_admin_ix_2(
        &mut env,
        &admin,
        encode_update_admin(&Pubkey::default()),
    );
    assert!(r.is_ok());

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &admin, encode_resolve_market());
    assert!(r.is_err(), "Cannot resolve after burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &admin, encode_withdraw_insurance());
    assert!(r.is_err(), "Cannot withdraw insurance after burn");
}

// ============================================================================
// Tests: full end-to-end staking flow
// ============================================================================

#[test]
fn test_e2e_stake_earn_unstake() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 500u64;
    let k: u128 = 1u128 << 64;
    let epoch_slots = 100u64;

    env.init_market_rewards(n, k, epoch_slots);

    // Set up staker
    let staker = Keypair::new();
    env.svm
        .airdrop(&staker.pubkey(), 10_000_000_000)
        .unwrap();

    // Stake collateral
    env.stake(&staker, 2_000_000);

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
    env.set_clock(300);

    // Claim stake rewards (no lockup requirement)
    let coin_ata = env.create_coin_ata(&staker.pubkey(), 0);
    env.claim_stake_rewards_to(&staker, &coin_ata);
    let stake_reward = env.read_token_balance(&coin_ata);
    // 500 * 2 = ~1000 (sole staker for 2 epochs)
    assert!(stake_reward >= 998 && stake_reward <= 1000, "Staker: ~1000, got {}", stake_reward);

    // Unstake collateral
    env.advance_blockhash();
    let (col_ata, _) = env.unstake_and_get_atas(&staker, 2_000_000);
    let col_balance = env.read_token_balance(&col_ata);
    assert_eq!(col_balance, 2_000_000, "Should get all collateral back");

    // Claim LP rewards
    let coin_ata_lp = env.create_coin_ata(&lp_owner.pubkey(), 0);
    env.claim_lp_rewards(&lp_owner, lp_idx, &coin_ata_lp);
    let lp_balance = env.read_token_balance(&coin_ata_lp);
    println!("LP rewards: {}", lp_balance);
    assert!(lp_balance > 0, "LP should get rewards from fees");
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
        data: encode_init_market_rewards(u64::MAX, 1u128 << 64, 100),
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
