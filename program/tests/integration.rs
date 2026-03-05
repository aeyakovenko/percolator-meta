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

// ============================================================================
// Rewards instruction encoders
// ============================================================================

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

        // COIN mint — authority is the rewards PDA
        let (mint_authority_pda, _) =
            Pubkey::find_program_address(&[b"coin_mint_authority", slab.as_ref()], &rewards_id);

        let coin_mint = Pubkey::new_unique();
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

    fn init_market_rewards(&mut self, n: u64, k: u128, total_contributed: u64) {
        let (mrc_pda, _) =
            Pubkey::find_program_address(&[b"mrc", self.slab.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.receipt_program, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market_rewards(n, k, total_contributed),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.payer.pubkey()),
            &[&self.payer],
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

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.slab, false),
                AccountMeta::new(mrc_pda, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.receipt_program, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_market_rewards(n, k, total_contributed),
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.payer.pubkey()),
            &[&self.payer],
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
// Tests: init_market_rewards
// ============================================================================

#[test]
fn test_init_market_rewards_happy_path() {
    let mut env = TestEnv::new();
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

    // Verify receipt_program (at offset 72)
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
    env.init_market_rewards(1000, 1u128 << 64, 10_000_000);

    // Second init should fail (PDA already exists)
    let result = env.try_init_market_rewards(1000, 1u128 << 64, 10_000_000);
    assert!(result.is_err(), "Double init should fail");
}

#[test]
fn test_init_market_rewards_k_exceeds_max_fails() {
    let mut env = TestEnv::new();
    let max_k: u128 = 1_000_000u128 << 64;
    let result = env.try_init_market_rewards(1000, max_k + 1, 10_000_000);
    assert!(result.is_err(), "K > MAX_LP_COIN_PER_FEE_FP should fail");
}

#[test]
fn test_init_market_rewards_k_at_max_succeeds() {
    let mut env = TestEnv::new();
    let max_k: u128 = 1_000_000u128 << 64;
    env.init_market_rewards(1000, max_k, 10_000_000);
}

#[test]
fn test_init_market_rewards_wrong_mint_authority_fails() {
    let mut env = TestEnv::new();

    // Replace coin_mint with one that has the wrong authority
    let wrong_auth = Pubkey::new_unique();
    env.svm
        .set_account(
            env.coin_mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_with_authority(&wrong_auth),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_market_rewards(1000, 1u128 << 64, 10_000_000);
    assert!(result.is_err(), "Wrong mint_authority should fail");
}

#[test]
fn test_init_market_rewards_freeze_authority_fails() {
    let mut env = TestEnv::new();
    let freeze = Pubkey::new_unique();

    env.svm
        .set_account(
            env.coin_mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_with_freeze(&env.mint_authority_pda, &freeze),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_market_rewards(1000, 1u128 << 64, 10_000_000);
    assert!(result.is_err(), "Mint with freeze_authority should fail");
}

#[test]
fn test_init_market_rewards_no_mint_authority_fails() {
    let mut env = TestEnv::new();

    env.svm
        .set_account(
            env.coin_mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_no_authority(),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_market_rewards(1000, 1u128 << 64, 10_000_000);
    assert!(result.is_err(), "Mint with no authority should fail");
}

// ============================================================================
// Tests: claim_owner_rewards
// ============================================================================

#[test]
fn test_claim_owner_rewards_happy_path() {
    let mut env = TestEnv::new();
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
// Tests: full end-to-end flow
// ============================================================================

#[test]
fn test_e2e_market_creation_to_claim() {
    let mut env = TestEnv::new();
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
