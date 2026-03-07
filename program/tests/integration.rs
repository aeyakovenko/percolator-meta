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

fn encode_mint_reward(amount: u64) -> Vec<u8> {
    let mut data = vec![5u8]; // tag = IX_MINT_REWARD
    data.extend_from_slice(&amount.to_le_bytes());
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
                0, // trading_fee_bps
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

    fn init_market_rewards(&mut self, n: u64, epoch_slots: u64) {
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
            data: encode_init_market_rewards(n, epoch_slots),
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
                AccountMeta::new_readonly(sysvar::rent::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market_rewards(n, epoch_slots),
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

    fn mint_reward(&mut self, amount: u64, destination: &Pubkey) {
        self.try_mint_reward(amount, destination).expect("mint_reward failed");
    }

    fn try_mint_reward(&mut self, amount: u64, destination: &Pubkey) -> Result<(), String> {
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", self.coin_mint.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),  // [0] authority
                AccountMeta::new(self.coin_mint, false),              // [1] coin_mint
                AccountMeta::new_readonly(coin_cfg_pda, false),       // [2] coin_config
                AccountMeta::new(*destination, false),                // [3] destination
                AccountMeta::new_readonly(self.mint_authority_pda, false), // [4] mint_authority
                AccountMeta::new_readonly(spl_token::ID, false),      // [5] token_program
            ],
            data: encode_mint_reward(amount),
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

    fn try_mint_reward_with_signer(
        &mut self,
        signer: &Keypair,
        amount: u64,
        destination: &Pubkey,
    ) -> Result<(), String> {
        let (coin_cfg_pda, _) = Pubkey::find_program_address(
            &[b"coin_cfg", self.coin_mint.as_ref()],
            &self.rewards_id,
        );

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new(*destination, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_mint_reward(amount),
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
fn test_stake_additional_resets_lockup() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

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
    env.init_market_rewards(1000, epoch_slots); // N=1000/epoch

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
    env.init_market_rewards(1000, 100);

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
// Tests: claim_stake_rewards (without unstaking)
// ============================================================================

#[test]
fn test_claim_stake_rewards_no_lockup_required() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

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
// Tests: multi-user staking accumulator
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

    // Each gets half: 1000 * 1 / 2 = ~500 (rounding)
    assert!(bal_a >= 499 && bal_a <= 500, "Alice gets ~50%, got {}", bal_a);
    assert!(bal_b >= 499 && bal_b <= 500, "Bob gets ~50%, got {}", bal_b);
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
// Tests: multi-user staggered withdrawal with balance verification
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

    // Slot 300: Both unstake fully (Alice has 1M left, Bob has 1M, pool = 2M)
    // Note: Alice's partial unstake resets lockup, so she needs another epoch
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
// Tests: mint_reward (governance-gated)
// ============================================================================

#[test]
fn test_mint_reward_happy_path() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let recipient = Pubkey::new_unique();
    let dest_ata = env.create_coin_ata(&recipient, 0);

    env.mint_reward(5000, &dest_ata);

    let balance = env.read_token_balance(&dest_ata);
    assert_eq!(balance, 5000, "Recipient should receive 5000 COIN");
}

#[test]
fn test_mint_reward_wrong_authority_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    let dest_ata = env.create_coin_ata(&attacker.pubkey(), 0);
    let result = env.try_mint_reward_with_signer(&attacker, 5000, &dest_ata);
    assert!(result.is_err(), "Non-authority should be rejected");
}

#[test]
fn test_mint_reward_zero_amount_fails() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let dest = Pubkey::new_unique();
    let dest_ata = env.create_coin_ata(&dest, 0);

    let result = env.try_mint_reward(0, &dest_ata);
    assert!(result.is_err(), "Minting 0 should fail");
}

#[test]
fn test_mint_reward_multiple_recipients() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let alice_ata = env.create_coin_ata(&Pubkey::new_unique(), 0);
    let bob_ata = env.create_coin_ata(&Pubkey::new_unique(), 0);

    env.mint_reward(1000, &alice_ata);
    env.advance_blockhash();
    env.mint_reward(2000, &bob_ata);

    assert_eq!(env.read_token_balance(&alice_ata), 1000);
    assert_eq!(env.read_token_balance(&bob_ata), 2000);
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

    let user = Keypair::new();
    let user_idx = env.init_user(&user);
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
    // 5 = mint_reward (governance-gated COIN mint to any destination)
    //
    // Staked collateral can only be withdrawn by the staker who deposited it.
    // COIN is only minted, never transferred from user accounts.

    let mut env = TestEnv::new();
    env.init_coin_config();
    env.init_market_rewards(1000, 100);

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
    assert!(result.is_err(), "Attacker cannot steal stake rewards");

    // Attacker cannot unstake staker's collateral
    let result = env.try_unstake(&attacker, 1_000_000);
    assert!(result.is_err(), "Attacker cannot unstake others' collateral");

    // Attacker cannot mint_reward (not DAO authority)
    let result = env.try_mint_reward_with_signer(&attacker, 1000, &attacker_coin);
    assert!(result.is_err(), "Attacker cannot mint COIN via mint_reward");
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
fn test_e2e_stake_earn_unstake_and_mint_reward() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let n = 500u64;
    let epoch_slots = 100u64;

    env.init_market_rewards(n, epoch_slots);

    // Set up staker
    let staker = Keypair::new();
    env.svm
        .airdrop(&staker.pubkey(), 10_000_000_000)
        .unwrap();

    // Stake collateral
    env.stake(&staker, 2_000_000);

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

    // DAO mints reward to an LP (governance-voted)
    let lp_recipient = Pubkey::new_unique();
    let lp_coin_ata = env.create_coin_ata(&lp_recipient, 0);
    env.advance_blockhash();
    env.mint_reward(3000, &lp_coin_ata);
    assert_eq!(env.read_token_balance(&lp_coin_ata), 3000, "LP should get governance-voted reward");
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
    env.set_clock(300); // past lockup

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

#[test]
fn test_mint_reward_non_signer_rejected() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let dest = env.create_coin_ata(&Pubkey::new_unique(), 0);

    let (coin_cfg_pda, _) = Pubkey::find_program_address(
        &[b"coin_cfg", env.coin_mint.as_ref()],
        &env.rewards_id,
    );

    // Build instruction with authority NOT as signer
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(env.dao_authority.pubkey(), false), // NOT a signer
            AccountMeta::new(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new(dest, false),
            AccountMeta::new_readonly(env.mint_authority_pda, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: encode_mint_reward(100),
    };
    let payer = &env.payer;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(result.is_err(), "mint_reward without signer must fail");
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
    env.set_clock(300); // past lockup

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

    let (mrc_pda, _) =
        Pubkey::find_program_address(&[b"mrc", raw_slab.as_ref()], &env.rewards_id);
    let (coin_cfg_pda, _) = Pubkey::find_program_address(
        &[b"coin_cfg", env.coin_mint.as_ref()],
        &env.rewards_id,
    );
    let (stake_vault, _) = Pubkey::find_program_address(
        &[b"stake_vault", raw_slab.as_ref()],
        &env.rewards_id,
    );

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(env.dao_authority.pubkey(), true),
            AccountMeta::new_readonly(raw_slab, false),
            AccountMeta::new(mrc_pda, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(env.collateral_mint, false),
            AccountMeta::new(stake_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(1000, 100),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
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
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.payer.pubkey()),
        &[&env.payer],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("init_market2 failed");

    // Init rewards for market 2 (different N)
    let (mrc_pda2, _) =
        Pubkey::find_program_address(&[b"mrc", slab2.as_ref()], &env.rewards_id);
    let (coin_cfg_pda, _) = Pubkey::find_program_address(
        &[b"coin_cfg", env.coin_mint.as_ref()],
        &env.rewards_id,
    );
    let (stake_vault2, _) =
        Pubkey::find_program_address(&[b"stake_vault", slab2.as_ref()], &env.rewards_id);

    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(env.dao_authority.pubkey(), true),
            AccountMeta::new_readonly(slab2, false),
            AccountMeta::new(mrc_pda2, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(coin_cfg_pda, false),
            AccountMeta::new_readonly(env.collateral_mint, false),
            AccountMeta::new(stake_vault2, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: encode_init_market_rewards(2000, 100), // 2x rewards vs market 1
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
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
    env.set_clock(300); // past lockup

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
