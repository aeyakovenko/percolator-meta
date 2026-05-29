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

use governance_adapter::{
    authority_address as governance_authority_address, id as governance_program_id,
};
use litesvm::LiteSVM;
use solana_program_runtime::compute_budget::ComputeBudget;
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
use spl_token::{
    instruction::AuthorityType,
    state::{Account as TokenAccount, AccountState, Mint},
};
use std::path::PathBuf;

// Production market account length for the pinned Percolator v16 program.
const SLAB_LEN: usize = percolator_prog::constants::MARKET_ACCOUNT_LEN;

const PYTH_RECEIVER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x0c, 0xb7, 0xfa, 0xbb, 0x52, 0xf7, 0xa6, 0x48, 0xbb, 0x5b, 0x31, 0x7d, 0x9a, 0x01, 0x8b, 0x90,
    0x57, 0xcb, 0x02, 0x47, 0x74, 0xfa, 0xfe, 0x01, 0xe6, 0xc4, 0xdf, 0x98, 0xcc, 0x38, 0x58, 0x81,
]);

// All-zeros feed_id = Hyperp mode (no external oracle read at init)
const TEST_FEED_ID: [u8; 32] = [0u8; 32];

fn read_percolator_config(data: &[u8]) -> percolator_prog::state::WrapperConfigV16 {
    percolator_prog::state::read_market_config_mode_and_capacity(data)
        .expect("read percolator market config")
        .0
}

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

fn make_percolator_market_data(
    market_slab: &Pubkey,
    collateral_mint: &Pubkey,
    admin: &Pubkey,
    init_slot: u64,
) -> Vec<u8> {
    let initial_price = 1_000_000;
    let mut wrapper = percolator_prog::state::WrapperConfigV16::default();
    wrapper.admin = admin.to_bytes();
    wrapper.collateral_mint = collateral_mint.to_bytes();
    wrapper.base_unit_authority = admin.to_bytes();
    wrapper.last_good_oracle_slot = init_slot;
    wrapper.insurance_authority = admin.to_bytes();
    wrapper.insurance_operator = admin.to_bytes();
    wrapper.backing_bucket_authority = admin.to_bytes();
    wrapper.asset_authority = admin.to_bytes();
    wrapper.mark_authority = admin.to_bytes();
    wrapper.insurance_withdraw_max_bps = 10_000;
    wrapper.insurance_withdraw_deposits_only = 1;
    wrapper.insurance_withdraw_cooldown_slots = 1;
    wrapper.permissionless_resolve_stale_slots = 2_000;
    wrapper.force_close_delay_slots = 100;
    wrapper.oracle_mode = percolator_prog::constants::ORACLE_MODE_MANUAL;
    wrapper.mark_ewma_e6 = initial_price;
    wrapper.mark_ewma_last_slot = init_slot;
    wrapper.mark_ewma_halflife_slots = percolator_prog::constants::DEFAULT_MARK_EWMA_HALFLIFE_SLOTS;
    wrapper.oracle_target_price_e6 = initial_price;

    let mut data = vec![0u8; SLAB_LEN];
    percolator_prog::state::init_market_account_zero_copy(
        &mut data,
        &wrapper,
        {
            let mut cfg = percolator_prog::risk::V16Config::public_user_fund(1, 0, 10);
            cfg.min_nonzero_mm_req = 1;
            cfg.min_nonzero_im_req = 2;
            cfg.maintenance_margin_bps = 10_000;
            cfg.initial_margin_bps = 10_000;
            cfg.max_trading_fee_bps = 10_000;
            cfg.max_accrual_dt_slots = 1;
            cfg.min_funding_lifetime_slots = 1;
            cfg.max_price_move_bps_per_slot = 10_000;
            cfg.max_account_b_settlement_chunks = 1;
            cfg.max_bankrupt_close_chunks = 1;
            cfg.max_bankrupt_close_lifetime_slots = 1;
            cfg.public_b_chunk_atoms = 1;
            cfg
        },
        market_slab.to_bytes(),
        initial_price,
        init_slot,
    )
    .expect("manual percolator market init");
    data
}

// ============================================================================
// Percolator instruction encoders
// ============================================================================

fn encode_init_market(
    _admin: &Pubkey,
    _mint: &Pubkey,
    _feed_id: &[u8; 32],
    _trading_fee_bps: u64,
) -> Vec<u8> {
    let mut data = vec![0u8];
    data.extend_from_slice(&1u16.to_le_bytes()); // max_portfolio_assets
    data.extend_from_slice(&0u64.to_le_bytes()); // h_min
    data.extend_from_slice(&1u64.to_le_bytes()); // h_max
    data.extend_from_slice(&1_000_000u64.to_le_bytes()); // initial_price
    data.extend_from_slice(&1u128.to_le_bytes()); // min_nonzero_mm_req
    data.extend_from_slice(&2u128.to_le_bytes()); // min_nonzero_im_req
    data.extend_from_slice(&10_000u64.to_le_bytes()); // maintenance_margin_bps
    data.extend_from_slice(&10_000u64.to_le_bytes()); // initial_margin_bps
    data.extend_from_slice(&0u64.to_le_bytes()); // max_trading_fee_bps
    data.extend_from_slice(&0u64.to_le_bytes()); // trade_fee_base_bps
    data.extend_from_slice(&0u64.to_le_bytes()); // liquidation_fee_bps
    data.extend_from_slice(&1_000_000_000_000u128.to_le_bytes()); // liquidation_fee_cap
    data.extend_from_slice(&0u128.to_le_bytes()); // min_liquidation_abs
    data.extend_from_slice(&10_000u64.to_le_bytes()); // max_price_move_bps_per_slot
    data.extend_from_slice(&1u64.to_le_bytes()); // max_accrual_dt_slots
    data.extend_from_slice(&0u64.to_le_bytes()); // max_abs_funding_e9_per_slot
    data.extend_from_slice(&1u64.to_le_bytes()); // min_funding_lifetime_slots
    data.extend_from_slice(&1u64.to_le_bytes()); // max_account_b_settlement_chunks
    data.extend_from_slice(&1u64.to_le_bytes()); // max_bankrupt_close_chunks
    data.extend_from_slice(&1u64.to_le_bytes()); // max_bankrupt_close_lifetime_slots
    data.extend_from_slice(&1u128.to_le_bytes()); // public_b_chunk_atoms
    data.extend_from_slice(&0u128.to_le_bytes()); // maintenance_fee_per_slot
    data
}

fn encode_init_user(fee: u64) -> Vec<u8> {
    let _ = fee;
    vec![1u8]
}

fn encode_deposit(user_idx: u16, amount: u64) -> Vec<u8> {
    let _ = user_idx;
    let mut data = vec![3u8];
    data.extend_from_slice(&(amount as u128).to_le_bytes());
    data
}

fn encode_update_admin(new_admin: &Pubkey) -> Vec<u8> {
    // Tag 32 = UpdateAuthority, kind 0 = AUTHORITY_ADMIN (was tag 12 UpdateAdmin)
    let mut data = vec![32u8, 0u8];
    data.extend_from_slice(new_admin.as_ref());
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

fn encode_set_oracle_authority(new_authority: &Pubkey) -> Vec<u8> {
    let mut data = vec![16u8];
    data.extend_from_slice(new_authority.as_ref());
    data
}

fn encode_resolve_market() -> Vec<u8> {
    vec![19u8]
}

fn encode_withdraw_insurance() -> Vec<u8> {
    let mut data = vec![41u8];
    data.extend_from_slice(&1u128.to_le_bytes());
    data
}

fn encode_admin_force_close(user_idx: u16) -> Vec<u8> {
    let mut data = vec![21u8];
    data.extend_from_slice(&user_idx.to_le_bytes());
    data
}

fn encode_set_insurance_withdraw_policy(
    _authority: &Pubkey,
    _min_withdraw_base: u128,
    max_withdraw_bps: u64,
    cooldown_slots: u64,
) -> Vec<u8> {
    let mut data = vec![33u8];
    data.extend_from_slice(&(max_withdraw_bps as u16).to_le_bytes());
    data.push(1u8); // deposits_only = true
    data.extend_from_slice(&cooldown_slots.to_le_bytes());
    data
}

fn encode_topup_insurance(amount: u64) -> Vec<u8> {
    let mut data = vec![9u8];
    data.extend_from_slice(&(amount as u128).to_le_bytes());
    data
}

// ============================================================================
// Rewards instruction encoders
// ============================================================================

fn encode_init_coin_config(bootstrap_delay_slots: u64) -> Vec<u8> {
    let mut data = vec![3u8]; // tag = IX_INIT_COIN_CONFIG
    data.extend_from_slice(&bootstrap_delay_slots.to_le_bytes());
    data
}

fn encode_governance_init_authority() -> Vec<u8> {
    vec![0u8]
}

fn encode_governance_init_coin_config(bootstrap_delay_slots: u64) -> Vec<u8> {
    let mut data = vec![1u8];
    data.extend_from_slice(&bootstrap_delay_slots.to_le_bytes());
    data
}

fn encode_governance_mint_reward(amount: u64) -> Vec<u8> {
    let mut data = vec![4u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn encode_governance_transfer_mint_authority() -> Vec<u8> {
    vec![6u8]
}

fn encode_governance_activate_live() -> Vec<u8> {
    vec![7u8]
}

fn encode_init_percolator_market(percolator_init_data: Vec<u8>) -> Vec<u8> {
    let mut data = vec![19u8];
    data.extend_from_slice(&percolator_init_data);
    data
}

fn encode_genesis_deposit(amount: u64) -> Vec<u8> {
    let mut data = vec![22u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn encode_genesis_withdraw() -> Vec<u8> {
    // Unified withdraw (tag 23); finalized/pre-kickstart phases take no pulls.
    encode_genesis_bootstrap_withdraw(0, 0, 0)
}

fn encode_governance_init_genesis_bootstrap(reward_supply: u64) -> Vec<u8> {
    let mut data = vec![10u8];
    data.extend_from_slice(&reward_supply.to_le_bytes());
    data
}

fn encode_governance_finalize_genesis() -> Vec<u8> {
    vec![12u8]
}

fn encode_governance_draw_genesis_surplus(amount: u64) -> Vec<u8> {
    let mut data = vec![13u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn encode_governance_kickstart_genesis_market(domain: u8, expiry_slot: u64) -> Vec<u8> {
    let mut data = vec![14u8];
    data.push(domain);
    data.extend_from_slice(&expiry_slot.to_le_bytes());
    data
}

fn encode_governance_recover_genesis_market(kind: u8, domain: u8, amount: u64) -> Vec<u8> {
    let mut data = vec![15u8];
    data.push(kind);
    data.push(domain);
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn encode_init_genesis_distribution(proposal_id: u64) -> Vec<u8> {
    // Proposals carry no per-proposal amount: each candidate is for the full
    // reward_supply (winner-take-all). Data = [tag, proposal_id].
    let mut data = vec![29u8];
    data.extend_from_slice(&proposal_id.to_le_bytes());
    data
}

/// Vote action encoding: 1 = back this proposal, 2 = retract. There is no
/// separate vote PDA — the ballot lives on the voter's GenesisPosition.
fn encode_vote_genesis_distribution(action: u8) -> Vec<u8> {
    vec![30u8, action]
}

/// trigger_genesis_distribution (tag 24) carries no amount: it mints the full
/// reward_supply to the winning proposal's destination. Permissionless.
fn encode_trigger_genesis_distribution() -> Vec<u8> {
    vec![24u8]
}

fn encode_governance_approve_builder(
    code_hash: [u8; 32],
    terms_hash: [u8; 32],
    enabled: bool,
) -> Vec<u8> {
    let mut data = vec![16u8];
    data.extend_from_slice(&code_hash);
    data.extend_from_slice(&terms_hash);
    data.push(enabled as u8);
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
    coin_mint: Pubkey,
    mint_authority_pda: Pubkey,
    account_count: u16,
    percolator_portfolios: Vec<Pubkey>,
}

impl TestEnv {
    fn new() -> Self {
        Self::new_with_governance_bootstrap(true)
    }

    fn new_without_governance_bootstrap() -> Self {
        Self::new_with_governance_bootstrap(false)
    }

    fn new_meta_only() -> Self {
        Self::new_with_options(true, false)
    }

    fn new_with_governance_bootstrap(bootstrap_governance: bool) -> Self {
        Self::new_with_options(bootstrap_governance, true)
    }

    fn new_with_options(bootstrap_governance: bool, init_percolator: bool) -> Self {
        let mut svm = LiteSVM::new().with_compute_budget(ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..ComputeBudget::default()
        });

        let percolator_id = percolator_prog::id();
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

        // COIN mint starts under DAO authority; bootstrap then hands minting to
        // the rewards PDA after the governance adapter controller is recorded.
        let coin_mint = Pubkey::new_unique();
        let (mint_authority_pda, _) = Pubkey::find_program_address(
            &[b"coin_mint_authority", coin_mint.as_ref()],
            &rewards_id,
        );
        let (governance_authority_pda, _) = governance_authority_address(&rewards_id, &coin_mint);
        svm.set_account(
            coin_mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_with_authority(&dao_authority.pubkey()),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

        if bootstrap_governance {
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

            let ix = spl_token::instruction::set_authority(
                &spl_token::ID,
                &coin_mint,
                Some(&mint_authority_pda),
                AuthorityType::MintTokens,
                &dao_authority.pubkey(),
                &[],
            )
            .unwrap();
            let tx = Transaction::new_signed_with_payer(
                &[ix],
                Some(&dao_authority.pubkey()),
                &[&dao_authority],
                svm.latest_blockhash(),
            );
            svm.send_transaction(tx)
                .expect("mint authority handoff failed");
        }

        if init_percolator {
            let mut slab_account = svm.get_account(&slab).expect("slab account missing");
            slab_account.data =
                make_percolator_market_data(&slab, &collateral_mint, &payer.pubkey(), 100);
            svm.set_account(slab, slab_account).unwrap();
        }

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
            coin_mint,
            mint_authority_pda,
            account_count: 0,
            percolator_portfolios: Vec::new(),
        }
    }

    fn try_init_governance_authority(&mut self, signer: &Keypair) -> Result<(), String> {
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_governance_init_authority(),
        };
        self.svm.expire_blockhash();
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

    fn handoff_mint_authority_to_rewards(&mut self) {
        let ix = spl_token::instruction::set_authority(
            &spl_token::ID,
            &self.coin_mint,
            Some(&self.mint_authority_pda),
            AuthorityType::MintTokens,
            &self.dao_authority.pubkey(),
            &[],
        )
        .unwrap();
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("mint authority handoff failed");
    }

    fn init_coin_config(&mut self) {
        self.init_coin_config_with_delay(0);
    }

    fn init_coin_config_with_delay(&mut self, bootstrap_delay_slots: u64) {
        let coin_mint = self.coin_mint;
        self.try_init_coin_config_with_mint_and_delay(&coin_mint, bootstrap_delay_slots)
            .expect("init_coin_config failed");
    }

    fn try_init_coin_config_direct_with_signers(
        &mut self,
        payer: &Keypair,
        authority: &Keypair,
        coin_mint: &Pubkey,
    ) -> Result<(), String> {
        self.try_init_coin_config_direct_with_signers_and_delay(payer, authority, coin_mint, 0)
    }

    fn try_init_coin_config_direct_with_signers_and_delay(
        &mut self,
        payer: &Keypair,
        authority: &Keypair,
        coin_mint: &Pubkey,
        bootstrap_delay_slots: u64,
    ) -> Result<(), String> {
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", coin_mint.as_ref()], &self.rewards_id);

        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(authority.pubkey(), true),
                AccountMeta::new_readonly(*coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_coin_config(bootstrap_delay_slots),
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
        self.try_init_coin_config_with_mint_and_delay(coin_mint, 0)
    }

    fn try_init_coin_config_with_mint_and_delay(
        &mut self,
        coin_mint: &Pubkey,
        bootstrap_delay_slots: u64,
    ) -> Result<(), String> {
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", coin_mint.as_ref()], &self.rewards_id);

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
            data: encode_governance_init_coin_config(bootstrap_delay_slots),
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

    fn coin_cfg_pda(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"coin_cfg", self.coin_mint.as_ref()], &self.rewards_id).0
    }

    fn try_activate_live(&mut self, signer: &Keypair) -> Result<(), String> {
        let coin_cfg_pda = self.coin_cfg_pda();
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new(coin_cfg_pda, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_governance_activate_live(),
        };
        self.svm.expire_blockhash();
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

    fn activate_live(&mut self) {
        let signer = Keypair::from_bytes(&self.dao_authority.to_bytes()).unwrap();
        self.try_activate_live(&signer)
            .expect("activate_live failed");
    }

    fn market_admin_pda(&self) -> Pubkey {
        Pubkey::find_program_address(
            &[b"percolator_market_admin", self.coin_mint.as_ref()],
            &self.rewards_id,
        )
        .0
    }

    fn genesis_cfg_pda(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"genesis_cfg", self.coin_mint.as_ref()], &self.rewards_id).0
    }

    fn genesis_vault_pda(&self) -> Pubkey {
        Pubkey::find_program_address(
            &[b"genesis_vault", self.coin_mint.as_ref()],
            &self.rewards_id,
        )
        .0
    }

    fn genesis_position_pda(&self, user: &Pubkey) -> Pubkey {
        let genesis_cfg = self.genesis_cfg_pda();
        Pubkey::find_program_address(
            &[b"genesis_position", genesis_cfg.as_ref(), user.as_ref()],
            &self.rewards_id,
        )
        .0
    }

    fn genesis_distribution_pda(&self, proposal_id: u64) -> Pubkey {
        let genesis_cfg = self.genesis_cfg_pda();
        Pubkey::find_program_address(
            &[
                b"genesis_distribution",
                genesis_cfg.as_ref(),
                &proposal_id.to_le_bytes(),
            ],
            &self.rewards_id,
        )
        .0
    }

    fn builder_approval_pda(&self, builder_program: &Pubkey, code_hash: &[u8; 32]) -> Pubkey {
        Pubkey::find_program_address(
            &[
                b"builder_approval",
                self.coin_mint.as_ref(),
                builder_program.as_ref(),
                code_hash,
            ],
            &self.rewards_id,
        )
        .0
    }

    fn init_genesis_bootstrap(&mut self, reward_supply: u64) {
        self.try_init_genesis_bootstrap(reward_supply)
            .expect("init_genesis_bootstrap failed");
    }

    fn try_init_genesis_bootstrap(&mut self, reward_supply: u64) -> Result<(), String> {
        let signer = Keypair::from_bytes(&self.dao_authority.to_bytes()).unwrap();
        self.try_init_genesis_bootstrap_with_signer(&signer, reward_supply)
    }

    fn try_init_genesis_bootstrap_with_signer(
        &mut self,
        signer: &Keypair,
        reward_supply: u64,
    ) -> Result<(), String> {
        let coin_cfg = self.coin_cfg_pda();
        let genesis_cfg = self.genesis_cfg_pda();
        let genesis_vault = self.genesis_vault_pda();
        let market_admin = self.market_admin_pda();
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg, false),
                AccountMeta::new_readonly(self.collateral_mint, false),
                AccountMeta::new(genesis_cfg, false),
                AccountMeta::new(genesis_vault, false),
                AccountMeta::new(market_admin, false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(sysvar::rent::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_governance_init_genesis_bootstrap(reward_supply),
        };
        self.svm.expire_blockhash();
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

    fn genesis_deposit(&mut self, user: &Keypair, amount: u64) {
        self.try_genesis_deposit(user, amount)
            .expect("genesis_deposit failed");
    }

    fn try_genesis_deposit(&mut self, user: &Keypair, amount: u64) -> Result<(), String> {
        let collateral_mint = self.collateral_mint;
        let user_ata = self.create_ata(&collateral_mint, &user.pubkey(), amount);
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new(self.genesis_cfg_pda(), false),
                AccountMeta::new(self.genesis_position_pda(&user.pubkey()), false),
                AccountMeta::new(user_ata, false),
                AccountMeta::new(self.genesis_vault_pda(), false),
                AccountMeta::new_readonly(spl_token::ID, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_genesis_deposit(amount),
        };
        self.svm.expire_blockhash();
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

    fn init_genesis_distribution(&mut self, proposal_id: u64, destination: &Pubkey) {
        self.try_init_genesis_distribution(proposal_id, destination)
            .expect("init genesis distribution failed");
    }

    fn try_init_genesis_distribution(
        &mut self,
        proposal_id: u64,
        destination: &Pubkey,
    ) -> Result<(), String> {
        let payer = Keypair::from_bytes(&self.dao_authority.to_bytes()).unwrap();
        self.try_init_genesis_distribution_with_payer(&payer, proposal_id, destination)
    }

    fn try_init_genesis_distribution_with_payer(
        &mut self,
        payer: &Keypair,
        proposal_id: u64,
        destination: &Pubkey,
    ) -> Result<(), String> {
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new_readonly(self.genesis_cfg_pda(), false),
                AccountMeta::new(self.genesis_distribution_pda(proposal_id), false),
                AccountMeta::new_readonly(*destination, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_genesis_distribution(proposal_id),
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[payer],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    /// Back a proposal (action 1). One vote per voter; switching proposals
    /// requires a retract first.
    fn vote_genesis_distribution(&mut self, voter: &Keypair, proposal_id: u64) {
        self.try_vote_genesis_distribution(voter, proposal_id)
            .expect("vote genesis distribution failed");
    }

    fn try_vote_genesis_distribution(
        &mut self,
        voter: &Keypair,
        proposal_id: u64,
    ) -> Result<(), String> {
        self.send_genesis_vote(voter, proposal_id, 1)
    }

    fn retract_genesis_vote(&mut self, voter: &Keypair, proposal_id: u64) -> Result<(), String> {
        self.send_genesis_vote(voter, proposal_id, 2)
    }

    // Shared ballot ix: the ballot lives on the GenesisPosition (no vote PDA),
    // and genesis_config is writable because it holds the running global tallies.
    fn send_genesis_vote(
        &mut self,
        voter: &Keypair,
        proposal_id: u64,
        action: u8,
    ) -> Result<(), String> {
        let proposal = self.genesis_distribution_pda(proposal_id);
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(voter.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new(self.genesis_cfg_pda(), false),
                AccountMeta::new(self.genesis_position_pda(&voter.pubkey()), false),
                AccountMeta::new(proposal, false),
            ],
            data: encode_vote_genesis_distribution(action),
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&voter.pubkey()),
            &[voter],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    /// Permissionless winner-take-all trigger (rewards tag 24). Anyone may fire it
    /// once a proposal has a quorum-valid weighted majority; it mints the FULL
    /// reward_supply to the proposal's destination. There is no governance signer.
    fn trigger_genesis_distribution(
        &mut self,
        cranker: &Keypair,
        proposal_id: u64,
        destination: &Pubkey,
    ) {
        self.try_trigger_genesis_distribution(cranker, proposal_id, destination)
            .expect("genesis trigger failed");
    }

    fn try_trigger_genesis_distribution(
        &mut self,
        cranker: &Keypair,
        proposal_id: u64,
        destination: &Pubkey,
    ) -> Result<(), String> {
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(cranker.pubkey(), true),
                AccountMeta::new(self.genesis_cfg_pda(), false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new(*destination, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new(self.genesis_distribution_pda(proposal_id), false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_trigger_genesis_distribution(),
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&cranker.pubkey()),
            &[cranker],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
    }

    fn finalize_genesis(&mut self) {
        self.try_finalize_genesis()
            .expect("finalize genesis failed");
    }

    fn try_finalize_genesis(&mut self) -> Result<(), String> {
        let signer = Keypair::from_bytes(&self.dao_authority.to_bytes()).unwrap();
        self.try_finalize_genesis_with_signer(&signer)
    }

    fn try_finalize_genesis_with_signer(&mut self, signer: &Keypair) -> Result<(), String> {
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new(self.genesis_cfg_pda(), false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
            ],
            data: encode_governance_finalize_genesis(),
        };
        self.svm.expire_blockhash();
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

    fn genesis_withdraw(&mut self, user: &Keypair) -> Pubkey {
        self.try_genesis_withdraw(user)
            .expect("genesis_withdraw failed")
    }

    fn try_genesis_withdraw(&mut self, user: &Keypair) -> Result<Pubkey, String> {
        let collateral_mint = self.collateral_mint;
        let user_ata = self.create_ata(&collateral_mint, &user.pubkey(), 0);
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(user.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new(self.genesis_cfg_pda(), false),
                AccountMeta::new(self.genesis_position_pda(&user.pubkey()), false),
                AccountMeta::new(user_ata, false),
                AccountMeta::new(self.genesis_vault_pda(), false),
                AccountMeta::new_readonly(self.market_admin_pda(), false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_genesis_withdraw(),
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&user.pubkey()),
            &[user],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| user_ata)
            .map_err(|e| format!("{:?}", e))
    }

    fn draw_genesis_surplus(&mut self, amount: u64, destination: &Pubkey) {
        let signer = Keypair::from_bytes(&self.dao_authority.to_bytes()).unwrap();
        self.try_draw_genesis_surplus_with_signer(&signer, amount, destination)
            .expect("draw_genesis_surplus failed");
    }

    fn try_draw_genesis_surplus_with_signer(
        &mut self,
        signer: &Keypair,
        amount: u64,
        destination: &Pubkey,
    ) -> Result<(), String> {
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.genesis_cfg_pda(), false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new(*destination, false),
                AccountMeta::new(self.genesis_vault_pda(), false),
                AccountMeta::new_readonly(self.market_admin_pda(), false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_governance_draw_genesis_surplus(amount),
        };
        self.svm.expire_blockhash();
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

    fn init_futarchy_percolator_market(&mut self) -> (Pubkey, Pubkey) {
        let slab = Pubkey::new_unique();
        self.svm
            .set_account(
                slab,
                Account {
                    lamports: 1_000_000_000,
                    data: vec![0u8; SLAB_LEN],
                    owner: self.percolator_id,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        let (vault_authority, _) =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &self.percolator_id);
        self.svm
            .set_account(
                vault_authority,
                Account {
                    lamports: 1_000_000,
                    data: vec![],
                    owner: solana_sdk::system_program::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        let vault = Pubkey::new_unique();
        self.svm
            .set_account(
                vault,
                Account {
                    lamports: 1_000_000,
                    data: make_token_account_data(&self.collateral_mint, &vault_authority, 0),
                    owner: spl_token::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        let ix = Instruction {
            program_id: self.rewards_id,
            accounts: vec![
                AccountMeta::new(self.payer.pubkey(), true),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new(self.market_admin_pda(), false),
                AccountMeta::new(slab, false),
                AccountMeta::new_readonly(self.collateral_mint, false),
                AccountMeta::new_readonly(self.percolator_id, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            ],
            data: encode_init_percolator_market(encode_init_market(
                &self.market_admin_pda(),
                &self.collateral_mint,
                &TEST_FEED_ID,
                0,
            )),
        };
        let tx = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
                ix,
            ],
            Some(&self.payer.pubkey()),
            &[&self.payer],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("init futarchy percolator market failed");
        (slab, vault)
    }

    fn install_manual_futarchy_market_for_test(&mut self) -> (Pubkey, Pubkey) {
        let slab = Pubkey::new_unique();
        self.svm
            .set_account(
                slab,
                Account {
                    lamports: 1_000_000_000,
                    data: make_percolator_market_data(
                        &slab,
                        &self.collateral_mint,
                        &self.market_admin_pda(),
                        100,
                    ),
                    owner: self.percolator_id,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        let (vault_authority, _) =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &self.percolator_id);
        let vault = Pubkey::new_unique();
        self.svm
            .set_account(
                vault,
                Account {
                    lamports: 1_000_000,
                    data: make_token_account_data(&self.collateral_mint, &vault_authority, 0),
                    owner: spl_token::ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        (slab, vault)
    }

    fn kickstart_genesis_market(&mut self, slab: &Pubkey, percolator_vault: &Pubkey) {
        let (percolator_vault_pda, _) =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &self.percolator_id);
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new(self.genesis_cfg_pda(), false),
                AccountMeta::new_readonly(self.market_admin_pda(), false),
                AccountMeta::new(*slab, false),
                AccountMeta::new(self.genesis_vault_pda(), false),
                AccountMeta::new(*percolator_vault, false),
                AccountMeta::new_readonly(percolator_vault_pda, false),
                AccountMeta::new_readonly(self.percolator_id, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_governance_kickstart_genesis_market(0, 10_000),
        };
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
                ix,
            ],
            Some(&self.dao_authority.pubkey()),
            &[&self.dao_authority],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("kickstart genesis market failed");
    }

    fn approve_builder(
        &mut self,
        builder_program: &Pubkey,
        code_hash: [u8; 32],
        terms_hash: [u8; 32],
        enabled: bool,
    ) {
        let signer = Keypair::from_bytes(&self.dao_authority.to_bytes()).unwrap();
        self.try_approve_builder_with_signer(
            &signer,
            builder_program,
            code_hash,
            terms_hash,
            enabled,
        )
        .expect("approve builder failed");
    }

    fn try_approve_builder_with_signer(
        &mut self,
        signer: &Keypair,
        builder_program: &Pubkey,
        code_hash: [u8; 32],
        terms_hash: [u8; 32],
        enabled: bool,
    ) -> Result<(), String> {
        let approval = self.builder_approval_pda(builder_program, &code_hash);
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new_readonly(*builder_program, false),
                AccountMeta::new(approval, false),
                AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
                AccountMeta::new_readonly(sysvar::clock::ID, false),
            ],
            data: encode_governance_approve_builder(code_hash, terms_hash, enabled),
        };
        self.svm.expire_blockhash();
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

    fn try_governance_unknown_tag(&mut self, signer: &Keypair, tag: u8) -> Result<(), String> {
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![AccountMeta::new(signer.pubkey(), true)],
            data: vec![tag],
        };
        self.svm.expire_blockhash();
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

    fn try_governance_percolator_admin_raw(
        &mut self,
        signer: &Keypair,
        percolator_ix_data: Vec<u8>,
    ) -> Result<(), String> {
        let mut data = vec![9u8];
        data.extend_from_slice(&percolator_ix_data);
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new_readonly(self.market_admin_pda(), false),
                AccountMeta::new_readonly(self.percolator_id, false),
            ],
            data,
        };
        self.svm.expire_blockhash();
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

    fn advance_blockhash(&mut self) {
        self.svm.expire_blockhash();
    }

    fn read_token_balance(&self, account: &Pubkey) -> u64 {
        let data = self.svm.get_account(account).unwrap();
        let token = TokenAccount::unpack(&data.data).unwrap();
        token.amount
    }

    fn read_mint(&self, mint: &Pubkey) -> Mint {
        let data = self.svm.get_account(mint).unwrap();
        Mint::unpack(&data.data).unwrap()
    }

    fn init_user(&mut self, owner: &Keypair) -> u16 {
        let idx = self.account_count;
        self.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let portfolio = Pubkey::new_unique();
        let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(1)
            .expect("portfolio account len");
        self.svm
            .set_account(
                portfolio,
                Account {
                    lamports: 1_000_000,
                    data: vec![0u8; portfolio_len],
                    owner: self.percolator_id,
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
                AccountMeta::new(portfolio, false),
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
        self.percolator_portfolios.push(portfolio);
        self.account_count += 1;
        idx
    }

    fn deposit(&mut self, owner: &Keypair, idx: u16, amount: u64) {
        let col_mint = self.collateral_mint;
        let ata = self.create_ata(&col_mint, &owner.pubkey(), amount);
        let portfolio = *self
            .percolator_portfolios
            .get(idx as usize)
            .expect("missing test portfolio");
        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(self.slab, false),
                AccountMeta::new(portfolio, false),
                AccountMeta::new(ata, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
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

    fn set_token_balance_for_test(&mut self, account: &Pubkey, amount: u64) {
        let mut account_data = self
            .svm
            .get_account(account)
            .expect("token account missing");
        let mut token = TokenAccount::unpack(&account_data.data).unwrap();
        token.amount = amount;
        TokenAccount::pack(token, &mut account_data.data).unwrap();
        self.svm.set_account(*account, account_data).unwrap();
    }

    fn force_genesis_kicked_for_test(&mut self) {
        let genesis_cfg = self.genesis_cfg_pda();
        let mut account = self
            .svm
            .get_account(&genesis_cfg)
            .expect("genesis cfg missing");
        account.data[137] = 1;
        self.svm.set_account(genesis_cfg, account).unwrap();
    }

    fn install_executable_builder_for_test(&mut self, builder_program: Pubkey) {
        let bytes = std::fs::read(governance_path()).expect("read governance BPF for builder");
        self.svm.add_program(builder_program, &bytes);
    }

    fn set_clock(&mut self, slot: u64) {
        self.svm.set_sysvar(&Clock {
            slot,
            unix_timestamp: slot as i64,
            ..Clock::default()
        });
        self.svm.expire_blockhash();
    }

    fn try_governance_mint_reward(
        &mut self,
        signer: &Keypair,
        amount: u64,
        destination: &Pubkey,
    ) -> Result<(), String> {
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", self.coin_mint.as_ref()], &self.rewards_id);
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new(*destination, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_governance_mint_reward(amount),
        };
        self.svm.expire_blockhash();
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

    fn try_transfer_mint_authority(
        &mut self,
        signer: &Keypair,
        new_authority: &Pubkey,
    ) -> Result<(), String> {
        let (coin_cfg_pda, _) =
            Pubkey::find_program_address(&[b"coin_cfg", self.coin_mint.as_ref()], &self.rewards_id);
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(signer.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new(self.coin_mint, false),
                AccountMeta::new_readonly(coin_cfg_pda, false),
                AccountMeta::new_readonly(self.mint_authority_pda, false),
                AccountMeta::new_readonly(*new_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_governance_transfer_mint_authority(),
        };
        self.svm.expire_blockhash();
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

    fn percolator_vault_for_slab(&mut self, slab: &Pubkey) -> Pubkey {
        if *slab == self.slab {
            return self.vault;
        }

        let (vault_authority, _) =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &self.percolator_id);
        let (vault_token, _) =
            Pubkey::find_program_address(&[b"test_perc_vault", slab.as_ref()], &self.rewards_id);
        if self.svm.get_account(&vault_token).is_none() {
            self.svm
                .set_account(
                    vault_token,
                    Account {
                        lamports: 1_000_000,
                        data: make_token_account_data(&self.collateral_mint, &vault_authority, 0),
                        owner: spl_token::ID,
                        executable: false,
                        rent_epoch: 0,
                    },
                )
                .unwrap();
        }
        vault_token
    }

    /// Top up percolator's insurance fund directly via percolator::TopUpInsurance.
    /// Current percolator v16 gates this by the market insurance authority.
    fn topup_percolator_insurance(&mut self, slab: &Pubkey, amount: u64) {
        let col_mint = self.collateral_mint;
        let signer = self.payer.pubkey();
        let source_ata = self.create_ata(&col_mint, &signer, amount);

        let perc_vault = self.percolator_vault_for_slab(slab);

        let ix = Instruction {
            program_id: self.percolator_id,
            accounts: vec![
                AccountMeta::new(signer, true),
                AccountMeta::new(*slab, false),
                AccountMeta::new(source_ata, false),
                AccountMeta::new(perc_vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_topup_insurance(amount),
        };
        let cu_ix = ComputeBudgetInstruction::set_compute_unit_limit(400_000);
        self.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[cu_ix, ix],
            Some(&signer),
            &[&self.payer],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .expect("topup_percolator_insurance failed");
    }

    /// Read the current percolator insurance balance through the pinned v16 state view.
    fn percolator_insurance_balance(&self, slab: &Pubkey) -> u128 {
        let mut slab_data = self.svm.get_account(slab).unwrap().data;
        let (_, group) =
            percolator_prog::state::market_view_mut(&mut slab_data).expect("read market view");
        group.header.insurance.get()
    }

    /// Simulate a FAITHFUL insurance-side trading loss: debit the internal insurance
    /// counter, the per-domain insurance budgets (asset 0, long+short), the aggregate
    /// vault counter, AND the on-chain collateral token vault in lockstep — exactly
    /// the lockstep a real loss settlement applies. This keeps the slab internally
    /// consistent (passes `validate_shape`) while making Percolator's real
    /// recoverability caps the binding constraint on recovery: unlike a token-only
    /// mock, recovery is now bounded by the engine's post-loss insurance budgets, not
    /// by a hand-picked amount. `loss` must not exceed the current insurance.
    fn simulate_insurance_loss(&mut self, slab: &Pubkey, percolator_vault: &Pubkey, loss: u128) {
        let mut slab_account = self.svm.get_account(slab).expect("slab missing");
        {
            let (_, mut group) = percolator_prog::state::market_view_mut(&mut slab_account.data)
                .expect("read market view");
            let new_insurance = group.header.insurance.get() - loss;
            let new_vault = group.header.vault.get() - loss;
            group.header.insurance = percolator_prog::risk::V16PodU128::new(new_insurance);
            group.header.vault = percolator_prog::risk::V16PodU128::new(new_vault);
            // Re-derive the per-domain budgets from the post-loss insurance total
            // (kickstart credits insurance as long = total/2, short = remainder).
            let slot = &mut group.markets[0].engine;
            slot.insurance_domain_budget_long =
                percolator_prog::risk::V16PodU128::new(new_insurance / 2);
            slot.insurance_domain_budget_short =
                percolator_prog::risk::V16PodU128::new(new_insurance - new_insurance / 2);
        }
        self.svm.set_account(*slab, slab_account).unwrap();
        let token = self.read_token_balance(percolator_vault);
        self.set_token_balance_for_test(percolator_vault, token - loss as u64);
    }

    /// Futarchy-gated recover (through the governance adapter, signed by the
    /// governance authority): pulls `amount` of `kind`/`domain` principal from the
    /// market into the shared genesis_vault. Requires the COIN to be live.
    fn governance_recover(
        &mut self,
        slab: &Pubkey,
        percolator_vault: &Pubkey,
        kind: u8,
        domain: u8,
        amount: u64,
    ) -> Result<(), String> {
        let (pvp, _) =
            Pubkey::find_program_address(&[b"vault", slab.as_ref()], &self.percolator_id);
        let ix = Instruction {
            program_id: self.governance_id,
            accounts: vec![
                AccountMeta::new(self.dao_authority.pubkey(), true),
                AccountMeta::new(self.governance_authority_pda, false),
                AccountMeta::new_readonly(self.rewards_id, false),
                AccountMeta::new_readonly(self.coin_mint, false),
                AccountMeta::new_readonly(self.coin_cfg_pda(), false),
                AccountMeta::new_readonly(self.genesis_cfg_pda(), false),
                AccountMeta::new_readonly(self.market_admin_pda(), false),
                AccountMeta::new(*slab, false),
                AccountMeta::new(self.genesis_vault_pda(), false),
                AccountMeta::new(*percolator_vault, false),
                AccountMeta::new_readonly(pvp, false),
                AccountMeta::new_readonly(self.percolator_id, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data: encode_governance_recover_genesis_market(kind, domain, amount),
        };
        self.svm.expire_blockhash();
        let dao = Keypair::from_bytes(&self.dao_authority.to_bytes()).unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), ix],
            Some(&dao.pubkey()),
            &[&dao],
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?}", e))
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
    assert_eq!(cfg_account.data.len(), 72); // COIN_CFG_SIZE

    assert_eq!(&cfg_account.data[..8], b"CCFGV002");
    let stored_auth = Pubkey::new_from_array(cfg_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_auth, env.governance_authority_pda);
    let stored_start = u64::from_le_bytes(cfg_account.data[40..48].try_into().unwrap());
    assert_eq!(stored_start, 100);
    let stored_delay = u64::from_le_bytes(cfg_account.data[48..56].try_into().unwrap());
    assert_eq!(stored_delay, 0);
    let stored_live_slot = u64::from_le_bytes(cfg_account.data[56..64].try_into().unwrap());
    assert_eq!(stored_live_slot, 100);
    assert_eq!(cfg_account.data[64], 1); // live phase
}

#[test]
fn test_init_coin_config_records_configurable_bootstrap_delay() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);

    let cfg_account = env.svm.get_account(&env.coin_cfg_pda()).unwrap();
    assert_eq!(&cfg_account.data[..8], b"CCFGV002");
    let stored_auth = Pubkey::new_from_array(cfg_account.data[8..40].try_into().unwrap());
    assert_eq!(stored_auth, env.governance_authority_pda);
    let stored_start = u64::from_le_bytes(cfg_account.data[40..48].try_into().unwrap());
    let stored_delay = u64::from_le_bytes(cfg_account.data[48..56].try_into().unwrap());
    let stored_live_slot = u64::from_le_bytes(cfg_account.data[56..64].try_into().unwrap());
    assert_eq!(stored_start, 100);
    assert_eq!(stored_delay, 50);
    assert_eq!(stored_live_slot, 0);
    assert_eq!(cfg_account.data[64], 0); // bootstrap phase
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

#[test]
fn test_trusted_bootstrap_ceremony_flow() {
    let mut env = TestEnv::new();

    let governance_authority = env
        .svm
        .get_account(&env.governance_authority_pda)
        .expect("governance authority PDA should exist");
    assert_eq!(governance_authority.owner, env.governance_id);

    env.init_coin_config();
    let (coin_cfg_pda, _) =
        Pubkey::find_program_address(&[b"coin_cfg", env.coin_mint.as_ref()], &env.rewards_id);
    let coin_cfg = env
        .svm
        .get_account(&coin_cfg_pda)
        .expect("coin config must exist");
    let stored_auth = Pubkey::new_from_array(coin_cfg.data[8..40].try_into().unwrap());
    assert_eq!(stored_auth, env.governance_authority_pda);

    let dao = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();
    for tag in [2u8, 3u8, 5u8] {
        assert!(
            env.try_governance_unknown_tag(&dao, tag).is_err(),
            "legacy governance staking tag {tag} must stay disabled"
        );
    }
    for tag in [0u8, 1u8, 2u8, 4u8, 5u8, 6u8, 7u8, 9u8] {
        let ix = Instruction {
            program_id: env.rewards_id,
            accounts: vec![],
            data: vec![tag],
        };
        env.svm.expire_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&env.payer.pubkey()),
            &[&env.payer],
            env.svm.latest_blockhash(),
        );
        assert!(
            env.svm.send_transaction(tx).is_err(),
            "legacy program staking tag {tag} must stay disabled"
        );
    }
}

#[test]
fn test_configurable_bootstrap_delay_blocks_live_actions_until_live() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);

    let dao = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();
    let dao_dest = env.create_coin_ata(&env.dao_authority.pubkey(), 0);
    let result = env.try_governance_mint_reward(&dao, 1, &dao_dest);
    assert!(
        result.is_err(),
        "live governance minting should be blocked while bootstrap phase is active"
    );

    env.set_clock(149);
    let result = env.try_activate_live(&dao);
    assert!(
        result.is_err(),
        "bootstrap activation should fail before configured delay elapses"
    );

    env.set_clock(150);
    env.activate_live();
    let cfg_account = env.svm.get_account(&env.coin_cfg_pda()).unwrap();
    let live_slot = u64::from_le_bytes(cfg_account.data[56..64].try_into().unwrap());
    assert_eq!(live_slot, 150);
    assert_eq!(cfg_account.data[64], 1);

    env.try_governance_mint_reward(&dao, 1, &dao_dest)
        .expect("governed reward mint should succeed after live activation");
    assert_eq!(env.read_token_balance(&dao_dest), 1);
}

#[test]
fn test_bootstrap_live_activation_requires_controller() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(10);
    env.set_clock(110);

    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();
    let result = env.try_activate_live(&attacker);
    assert!(
        result.is_err(),
        "non-controller must not be able to activate live phase"
    );

    let cfg_account = env.svm.get_account(&env.coin_cfg_pda()).unwrap();
    assert_eq!(cfg_account.data[64], 0);
}

#[test]
fn test_genesis_bootstrap_votes_distribution_withdrawal_and_surplus() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();

    env.genesis_deposit(&alice, 1);
    env.genesis_deposit(&bob, 3);
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 4);

    let cfg_account = env.svm.get_account(&env.genesis_cfg_pda()).unwrap();
    assert_eq!(
        u64::from_le_bytes(cfg_account.data[104..112].try_into().unwrap()),
        4
    );
    let alice_pos = env
        .svm
        .get_account(&env.genesis_position_pda(&alice.pubkey()))
        .unwrap();
    assert!(
        u64::from_le_bytes(alice_pos.data[56..64].try_into().unwrap()) > 0,
        "deposit records a start slot for time-weighting"
    );

    let alice_coin = env.create_coin_ata(&alice.pubkey(), 0);
    let cranker = Keypair::new();
    env.svm.airdrop(&cranker.pubkey(), 10_000_000_000).unwrap();
    let early = env.try_trigger_genesis_distribution(&cranker, 1, &alice_coin);
    assert!(
        early.is_err(),
        "distribution is blocked before genesis ends"
    );

    env.set_clock(150);
    env.activate_live();
    env.force_genesis_kicked_for_test(); // triggering now requires a kicked market
    let bob_coin = env.create_coin_ata(&bob.pubkey(), 0);
    env.init_genesis_distribution(1, &alice_coin);
    // No votes yet: the proposal lacks both a quorum and a weighted majority.
    let unapproved = env.try_trigger_genesis_distribution(&cranker, 1, &alice_coin);
    assert!(
        unapproved.is_err(),
        "triggering requires a quorum-valid weighted majority for the proposal"
    );
    // A competing proposal exists, but winner-take-all means only the one the
    // depositors actually back is minted the full supply. Both back proposal 1.
    env.init_genesis_distribution(2, &bob_coin);
    env.vote_genesis_distribution(&alice, 1);
    env.vote_genesis_distribution(&bob, 1);
    env.trigger_genesis_distribution(&cranker, 1, &alice_coin);
    assert_eq!(
        env.read_token_balance(&alice_coin),
        100,
        "the winning proposal is minted the full reward supply"
    );
    assert_eq!(env.read_token_balance(&bob_coin), 0, "the unbacked proposal mints nothing");
    let overmint = env.try_trigger_genesis_distribution(&cranker, 2, &bob_coin);
    assert!(
        overmint.is_err(),
        "winner-take-all: the genesis distribution executes exactly once"
    );
    env.force_genesis_kicked_for_test();
    env.finalize_genesis();

    let collateral_mint = env.collateral_mint;
    let donor_ata = env.create_ata(&collateral_mint, &env.dao_authority.pubkey(), 2);
    let xfer = spl_token::instruction::transfer(
        &spl_token::ID,
        &donor_ata,
        &env.genesis_vault_pda(),
        &env.dao_authority.pubkey(),
        &[],
        2,
    )
    .unwrap();
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[xfer],
        Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .expect("inject genesis surplus");

    let alice_base = env.genesis_withdraw(&alice);
    let bob_base = env.genesis_withdraw(&bob);
    assert_eq!(env.read_token_balance(&alice_base), 1);
    assert_eq!(env.read_token_balance(&bob_base), 3);
    let alice_pos = env
        .svm
        .get_account(&env.genesis_position_pda(&alice.pubkey()))
        .unwrap();
    assert_eq!(
        u64::from_le_bytes(alice_pos.data[56..64].try_into().unwrap()),
        0,
        "votes are worthless after principal claim"
    );

    let collateral_mint = env.collateral_mint;
    let dao_base = env.create_ata(&collateral_mint, &env.dao_authority.pubkey(), 0);
    env.draw_genesis_surplus(2, &dao_base);
    assert_eq!(env.read_token_balance(&dao_base), 2);
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 0);
}

#[test]
fn test_genesis_deposits_stay_open_until_voting_starts() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    let early = Keypair::new();
    let mid = Keypair::new();
    let late = Keypair::new();
    for user in [&early, &mid, &late] {
        env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    }

    // Joining stays open across the whole pre-live bootstrap phase, not a fixed
    // window: a deposit far past the old window still succeeds.
    env.genesis_deposit(&early, 1); // slot 100
    env.set_clock(140);
    env.genesis_deposit(&mid, 1); // well past the old 105-slot window
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 2);

    // Once the bootstrap delay elapses and the COIN goes live (voting starts),
    // joining closes.
    env.set_clock(150);
    env.activate_live();
    let late_deposit = env.try_genesis_deposit(&late, 1);
    assert!(
        late_deposit.is_err(),
        "joining closes once voting starts (COIN live)"
    );
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 2);
}

#[test]
fn test_genesis_distribution_creation_is_permissionless_but_bounded() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    let proposer = Keypair::new();
    env.svm.airdrop(&proposer.pubkey(), 10_000_000_000).unwrap();
    let destination = env.create_coin_ata(&proposer.pubkey(), 0);

    let early = env.try_init_genesis_distribution_with_payer(&proposer, 1, &destination);
    assert!(
        early.is_err(),
        "genesis allocation proposals are blocked until the COIN instance is live"
    );

    env.set_clock(150);
    env.activate_live();

    // The destination must be a COIN token account — proposals carry no amount
    // (each candidate is for the full reward_supply, winner-take-all), so the only
    // bound left on creation is the destination mint and PDA uniqueness.
    let collateral_mint = env.collateral_mint;
    let wrong_mint_destination = env.create_ata(&collateral_mint, &proposer.pubkey(), 0);
    let wrong_mint =
        env.try_init_genesis_distribution_with_payer(&proposer, 1, &wrong_mint_destination);
    assert!(
        wrong_mint.is_err(),
        "allocation destination must be a COIN token account"
    );

    // Anyone (not just governance) can create a candidate proposal.
    env.try_init_genesis_distribution_with_payer(&proposer, 1, &destination)
        .expect("permissionless proposer should be able to create a candidate proposal");

    let duplicate = env.try_init_genesis_distribution_with_payer(&proposer, 1, &destination);
    assert!(duplicate.is_err(), "proposal ids are one-shot PDAs");
}

#[test]
fn test_genesis_distribution_and_finalize_require_market_kickstart() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(1);
    env.init_genesis_bootstrap(100);

    let voter = Keypair::new();
    env.svm.airdrop(&voter.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&voter, 1); // slot 100
    env.set_clock(110); // live (delay 1) and age >= 2 for nonzero log-weight
    env.activate_live();

    let destination = env.create_coin_ata(&voter.pubkey(), 0);
    env.init_genesis_distribution(1, &destination);
    env.vote_genesis_distribution(&voter, 1);

    let cranker = Keypair::new();
    env.svm.airdrop(&cranker.pubkey(), 10_000_000_000).unwrap();

    // Neither triggering the distribution nor finalizing is allowed until the
    // pooled capital is actually deployed into the market at kickstart.
    let trigger_without_kick = env.try_trigger_genesis_distribution(&cranker, 1, &destination);
    assert!(
        trigger_without_kick.is_err(),
        "COIN cannot be distributed before the genesis market is kickstarted"
    );
    let finalize_without_kick = env.try_finalize_genesis();
    assert!(
        finalize_without_kick.is_err(),
        "genesis cannot finalize before pooled risk is deployed"
    );

    env.force_genesis_kicked_for_test();
    env.trigger_genesis_distribution(&cranker, 1, &destination);
    env.finalize_genesis();
}

#[test]
fn test_underfunded_genesis_haircut_is_order_independent_and_loop_proof() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(1);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 2); // slot 100
    env.genesis_deposit(&bob, 2);
    env.set_clock(110); // live (delay 1) and age >= 2 for nonzero log-weight
    env.activate_live();
    env.force_genesis_kicked_for_test(); // mint now requires a kicked market

    let destination = env.create_coin_ata(&alice.pubkey(), 0);
    env.init_genesis_distribution(1, &destination);
    env.vote_genesis_distribution(&alice, 1);
    env.vote_genesis_distribution(&bob, 1);
    let cranker = Keypair::new();
    env.svm.airdrop(&cranker.pubkey(), 10_000_000_000).unwrap();
    env.trigger_genesis_distribution(&cranker, 1, &destination);
    env.force_genesis_kicked_for_test();
    env.finalize_genesis();

    // The market lost half the pooled capital: vault holds 2 against 4 of
    // outstanding principal (a 50% health ratio).
    let genesis_vault = env.genesis_vault_pda();
    env.set_token_balance_for_test(&genesis_vault, 2);

    // Alice withdraws FIRST and gets her 50% pro-rata share. The full claim is
    // settled at the health ratio: pos.withdrawn becomes the whole 2 (the unpaid
    // half is her realized share of the loss), not just the 1 token paid.
    let alice_base = env.genesis_withdraw(&alice);
    assert_eq!(
        env.read_token_balance(&alice_base),
        1,
        "alice recovers her 50% pro-rata share"
    );
    let alice_pos = env
        .svm
        .get_account(&env.genesis_position_pda(&alice.pubkey()))
        .unwrap();
    assert_eq!(
        u64::from_le_bytes(alice_pos.data[48..56].try_into().unwrap()),
        2,
        "the full claim is settled at the health ratio; the unpaid half is realized as loss"
    );

    // Looping pays nothing more — the claim is settled, so the withdrawal race is
    // dead (under the old live pro-rata, repeated calls drained the shared vault).
    let alice_again = env.genesis_withdraw(&alice);
    assert_eq!(
        env.read_token_balance(&alice_again),
        0,
        "re-withdrawing yields nothing: no loop-amplified extraction"
    );

    // Bob, withdrawing SECOND, recovers the SAME fair 50% share — settling alice's
    // full claim kept the health ratio invariant. (Under the old code bob, going
    // second against the depleted vault, would have recovered 0.)
    let bob_base = env.genesis_withdraw(&bob);
    assert_eq!(
        env.read_token_balance(&bob_base),
        1,
        "bob (second) recovers the same 50% share, not a degraded one"
    );
}

#[test]
fn test_genesis_vote_records_are_nontransferable_and_strict_majority() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(10);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    let outsider = Keypair::new();
    for user in [&alice, &bob, &outsider] {
        env.svm.airdrop(&user.pubkey(), 10_000_000_000).unwrap();
    }

    env.genesis_deposit(&alice, 1);
    env.genesis_deposit(&bob, 1);
    env.set_clock(120);
    env.activate_live();
    env.force_genesis_kicked_for_test(); // triggering now requires a kicked market

    let dest1 = env.create_coin_ata(&alice.pubkey(), 0);
    let dest2 = env.create_coin_ata(&bob.pubkey(), 0);
    env.init_genesis_distribution(1, &dest1);
    env.init_genesis_distribution(2, &dest2);

    let cranker = Keypair::new();
    env.svm.airdrop(&cranker.pubkey(), 10_000_000_000).unwrap();

    // Only genesis depositors can vote; an outsider with no position is rejected.
    let outsider_vote = env.try_vote_genesis_distribution(&outsider, 1);
    assert!(
        outsider_vote.is_err(),
        "only genesis depositors with recorded vote units can vote"
    );

    // Both joined at slot 100 and vote at slot 120, so age = 20 and each base
    // unit weighs floor(log2(20)) = 4.
    env.vote_genesis_distribution(&alice, 1);
    assert_eq!(read_position_vote_weight(&env, &alice.pubkey()), 4, "alice weight = log2(20) * 1");

    // One of two equal depositors does not meet the principal quorum
    // (voted principal 1, outstanding 2 -> 1*2 == 2, not > 2).
    let one_backer = env.try_trigger_genesis_distribution(&cranker, 1, &dest1);
    assert!(
        one_backer.is_err(),
        "one of two equal depositors does not meet the principal quorum"
    );

    // One vote, one proposal: bob backs proposal 2, so the cast weight is split
    // 4/4 across the two candidates. Neither holds a strict majority.
    env.vote_genesis_distribution(&bob, 2);
    let p1 = env.svm.get_account(&env.genesis_distribution_pda(1)).unwrap();
    let p2 = env.svm.get_account(&env.genesis_distribution_pda(2)).unwrap();
    assert_eq!(u64::from_le_bytes(p1.data[80..88].try_into().unwrap()), 4, "prop1 support");
    assert_eq!(u64::from_le_bytes(p2.data[80..88].try_into().unwrap()), 4, "prop2 support");
    assert!(
        env.try_trigger_genesis_distribution(&cranker, 1, &dest1).is_err(),
        "a proposal with only half the cast weight cannot win"
    );

    // Bob cannot back a second proposal without retracting first (one vote rule).
    let double_vote = env.try_vote_genesis_distribution(&bob, 1);
    assert!(
        double_vote.is_err(),
        "a voter must retract before backing another proposal"
    );

    // Bob retracts proposal 2 and backs proposal 1: now prop1 holds all the cast
    // weight (8 of 8), a strict majority, and the quorum is met (principal 2 of 2).
    env.retract_genesis_vote(&bob, 2).expect("retract prop2");
    env.vote_genesis_distribution(&bob, 1);
    let p1 = env.svm.get_account(&env.genesis_distribution_pda(1)).unwrap();
    assert_eq!(
        u64::from_le_bytes(p1.data[80..88].try_into().unwrap()),
        8,
        "prop1 now holds both ballots' weight"
    );
    assert_eq!(read_position_vote_weight(&env, &bob.pubkey()), 4, "bob's ballot moved to prop1");

    // Permissionless winner-take-all: the full reward_supply is minted to prop1.
    env.trigger_genesis_distribution(&cranker, 1, &dest1);
    assert_eq!(env.read_token_balance(&dest1), 100);

    // The decision is over: the losing proposal cannot also be triggered, and the
    // winning one cannot be re-voted.
    let second_trigger = env.try_trigger_genesis_distribution(&cranker, 2, &dest2);
    assert!(second_trigger.is_err(), "winner-take-all: only one proposal mints the supply");
    let post_execute_vote = env.try_vote_genesis_distribution(&alice, 1);
    assert!(
        post_execute_vote.is_err(),
        "executed allocations cannot be re-voted"
    );

    let early_withdraw = env.try_genesis_withdraw(&alice);
    assert!(
        early_withdraw.is_err(),
        "genesis principal cannot be withdrawn before finalization"
    );

    env.force_genesis_kicked_for_test();
    env.finalize_genesis();
    let post_finalize_vote = env.try_vote_genesis_distribution(&alice, 1);
    assert!(
        post_finalize_vote.is_err(),
        "finalized genesis vote units are not reusable"
    );
}

#[test]
fn test_genesis_governance_surface_is_fixed_and_controller_gated() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(10);

    let dao = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    let unknown = env.try_governance_unknown_tag(&dao, 250);
    assert!(
        unknown.is_err(),
        "governance adapter has no catch-all executor"
    );

    let attacker_bootstrap = env.try_init_genesis_bootstrap_with_signer(&attacker, 100);
    assert!(
        attacker_bootstrap.is_err(),
        "non-controller cannot initialize genesis bootstrap"
    );

    env.init_genesis_bootstrap(100);
    let voter = Keypair::new();
    env.svm.airdrop(&voter.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&voter, 1);
    env.set_clock(120);
    env.activate_live();
    let destination = env.create_coin_ata(&voter.pubkey(), 0);
    env.init_genesis_distribution(1, &destination);
    env.vote_genesis_distribution(&voter, 1);

    // The distribution trigger is NOT governance-gated: it is permissionless on
    // the rewards program. A non-controller attacker can fire it once the quorum +
    // weighted-majority + kicked conditions hold, and it mints the full supply.
    env.force_genesis_kicked_for_test();
    env.trigger_genesis_distribution(&attacker, 1, &destination);
    assert_eq!(
        env.read_token_balance(&destination),
        100,
        "anyone may crank the winning distribution; it is not controller-gated"
    );

    // Finalize, by contrast, remains controller-gated.
    let attacker_finalize = env.try_finalize_genesis_with_signer(&attacker);
    assert!(
        attacker_finalize.is_err(),
        "non-controller cannot finalize genesis"
    );

    let collateral_mint = env.collateral_mint;
    let attacker_base = env.create_ata(&collateral_mint, &attacker.pubkey(), 0);
    let attacker_draw = env.try_draw_genesis_surplus_with_signer(&attacker, 1, &attacker_base);
    assert!(
        attacker_draw.is_err(),
        "non-controller cannot draw genesis surplus"
    );

    let builder_program = Pubkey::new_unique();
    let attacker_approval = env.try_approve_builder_with_signer(
        &attacker,
        &builder_program,
        [1u8; 32],
        [2u8; 32],
        true,
    );
    assert!(
        attacker_approval.is_err(),
        "non-controller cannot approve builder code"
    );

    let funding_tag = env.try_governance_percolator_admin_raw(&dao, vec![9u8]);
    assert!(
        funding_tag.is_err(),
        "governance percolator proxy rejects funding/withdrawal-style tags"
    );
    let custody_update =
        env.try_governance_percolator_admin_raw(&dao, encode_update_admin(&attacker.pubkey()));
    assert!(
        custody_update.is_err(),
        "generic futarchy admin cannot move Percolator custody authorities"
    );
}

#[test]
fn test_builder_code_approval_registry_is_governed_and_versioned() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config();
    let non_executable = Pubkey::new_unique();
    env.svm
        .set_account(
            non_executable,
            Account {
                lamports: 1_000_000,
                data: vec![],
                owner: solana_sdk::system_program::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let code_hash = [7u8; 32];
    let terms_hash = [9u8; 32];
    let dao = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();
    let rejected =
        env.try_approve_builder_with_signer(&dao, &non_executable, code_hash, terms_hash, true);
    assert!(
        rejected.is_err(),
        "builder approvals are only for executable BPF program accounts"
    );

    let builder_program = Pubkey::new_unique();
    env.install_executable_builder_for_test(builder_program);
    env.approve_builder(&builder_program, code_hash, terms_hash, true);

    let approval = env.builder_approval_pda(&builder_program, &code_hash);
    let account = env.svm.get_account(&approval).unwrap();
    assert_eq!(&account.data[..8], b"BLDAPP01");
    assert_eq!(
        Pubkey::new_from_array(account.data[8..40].try_into().unwrap()),
        env.coin_mint
    );
    assert_eq!(
        Pubkey::new_from_array(account.data[40..72].try_into().unwrap()),
        builder_program
    );
    assert_eq!(&account.data[72..104], &code_hash);
    assert_eq!(&account.data[104..136], &terms_hash);
    assert_eq!(account.data[144], 1);

    let new_terms_hash = [10u8; 32];
    env.approve_builder(&builder_program, code_hash, new_terms_hash, false);
    let account = env.svm.get_account(&approval).unwrap();
    assert_eq!(&account.data[104..136], &new_terms_hash);
    assert_eq!(account.data[144], 0);
}

#[test]
fn test_genesis_recovery_rejects_unneeded_ledger_accounts() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(1);
    env.init_genesis_bootstrap(100);
    let depositor = Keypair::new();
    env.svm
        .airdrop(&depositor.pubkey(), 10_000_000_000)
        .unwrap();
    env.genesis_deposit(&depositor, 1);
    env.set_clock(101);
    env.activate_live();
    env.force_genesis_kicked_for_test();

    let (slab, percolator_vault) = env.install_manual_futarchy_market_for_test();
    let (percolator_vault_pda, _) =
        Pubkey::find_program_address(&[b"vault", slab.as_ref()], &env.percolator_id);
    let extra_ledger = Pubkey::new_unique();
    env.svm
        .set_account(
            extra_ledger,
            Account {
                lamports: 1_000_000,
                data: vec![0u8; 8],
                owner: env.percolator_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let ix = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(env.dao_authority.pubkey(), true),
            AccountMeta::new(env.governance_authority_pda, false),
            AccountMeta::new_readonly(env.rewards_id, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new_readonly(env.genesis_cfg_pda(), false),
            AccountMeta::new_readonly(env.market_admin_pda(), false),
            AccountMeta::new(slab, false),
            AccountMeta::new(env.genesis_vault_pda(), false),
            AccountMeta::new(percolator_vault, false),
            AccountMeta::new_readonly(percolator_vault_pda, false),
            AccountMeta::new_readonly(env.percolator_id, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new(extra_ledger, false),
        ],
        data: encode_governance_recover_genesis_market(0, 0, 1),
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ix,
        ],
        Some(&env.dao_authority.pubkey()),
        &[&env.dao_authority],
        env.svm.latest_blockhash(),
    );
    let result = env.svm.send_transaction(tx);
    assert!(
        result.is_err(),
        "only backing-earnings recovery may include an engine ledger account"
    );
}

#[test]
fn test_genesis_bootstrap_kickstarts_market_50_50() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 2);
    env.genesis_deposit(&bob, 2);

    let (slab, percolator_vault) = env.init_futarchy_percolator_market();
    let header = read_percolator_config(&env.svm.get_account(&slab).unwrap().data);
    assert_eq!(
        Pubkey::new_from_array(header.admin),
        env.market_admin_pda(),
        "market admin is the futarchy PDA from creation"
    );

    env.kickstart_genesis_market(&slab, &percolator_vault);
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 0);
    assert_eq!(env.read_token_balance(&percolator_vault), 4);
    assert_eq!(
        env.percolator_insurance_balance(&slab),
        2,
        "half of genesis principal goes to insurance"
    );

    let cfg_account = env.svm.get_account(&env.genesis_cfg_pda()).unwrap();
    assert_eq!(cfg_account.data[137], 1, "genesis market was kicked");
    let late = Keypair::new();
    env.svm.airdrop(&late.pubkey(), 10_000_000_000).unwrap();
    let late_deposit = env.try_genesis_deposit(&late, 1);
    assert!(
        late_deposit.is_err(),
        "genesis deposits close once pooled capital is deployed"
    );

    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 0);
}

#[test]
fn test_governance_init_authority_requires_current_mint_authority() {
    let mut env = TestEnv::new_without_governance_bootstrap();
    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();

    let result = env.try_init_governance_authority(&attacker);
    assert!(
        result.is_err(),
        "attacker must not initialize governance authority for a mint they do not control"
    );
    assert!(
        env.svm.get_account(&env.governance_authority_pda).is_none(),
        "failed first mover must not create authority PDA"
    );

    let dao = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();
    env.try_init_governance_authority(&dao)
        .expect("mint authority should initialize governance adapter");
    env.handoff_mint_authority_to_rewards();
    env.init_coin_config();

    let authority = env
        .svm
        .get_account(&env.governance_authority_pda)
        .expect("authority account exists");
    assert_eq!(&authority.data[..8], b"GAUTH001");
    let stored_controller = Pubkey::new_from_array(authority.data[8..40].try_into().unwrap());
    assert_eq!(stored_controller, env.dao_authority.pubkey());
}
// ============================================================================
// Tests: governed reward mint lifecycle
// ============================================================================

#[test]
fn test_governance_reward_lifecycle_mint_and_transfer_authority() {
    let mut env = TestEnv::new();
    env.init_coin_config();

    let dao = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();
    let dao_dest = env.create_coin_ata(&env.dao_authority.pubkey(), 0);
    env.try_governance_mint_reward(&dao, 123, &dao_dest)
        .expect("controller should mint governed reward");
    assert_eq!(env.read_token_balance(&dao_dest), 123);

    let attacker = Keypair::new();
    env.svm.airdrop(&attacker.pubkey(), 10_000_000_000).unwrap();
    let attacker_dest = env.create_coin_ata(&attacker.pubkey(), 0);
    let result = env.try_governance_mint_reward(&attacker, 1, &attacker_dest);
    assert!(
        result.is_err(),
        "non-controller must not drive governed reward minting"
    );
    assert_eq!(env.read_token_balance(&attacker_dest), 0);

    let new_authority = Keypair::new();
    env.svm.airdrop(&new_authority.pubkey(), 1_000_000).unwrap();
    env.try_transfer_mint_authority(&dao, &new_authority.pubkey())
        .expect("controller should transfer mint authority");
    let mint = env.read_mint(&env.coin_mint);
    assert_eq!(
        mint.mint_authority,
        solana_sdk::program_option::COption::Some(new_authority.pubkey())
    );

    let result = env.try_governance_mint_reward(&dao, 1, &dao_dest);
    assert!(
        result.is_err(),
        "rewards PDA must stop minting after authority is transferred away"
    );
}

#[test]
fn test_bootstrap_phase_blocks_governed_reward_mint_until_live() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(10);

    let dao = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();
    let dao_dest = env.create_coin_ata(&env.dao_authority.pubkey(), 0);
    let result = env.try_governance_mint_reward(&dao, 1, &dao_dest);
    assert!(
        result.is_err(),
        "bootstrap phase must block governed reward minting"
    );
    assert_eq!(env.read_token_balance(&dao_dest), 0);

    env.set_clock(110);
    env.activate_live();
    env.try_governance_mint_reward(&dao, 1, &dao_dest)
        .expect("governed reward mint should succeed after live activation");
    assert_eq!(env.read_token_balance(&dao_dest), 1);
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
fn try_update_admin(env: &mut TestEnv, admin: &Keypair, new_admin: &Pubkey) -> Result<(), String> {
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
    assert!(
        result.is_ok(),
        "Admin should work before burn: {:?}",
        result
    );

    // Now burn admin
    env.advance_blockhash();
    let result = try_update_admin(&mut env, &admin, &Pubkey::default());
    assert!(
        result.is_ok(),
        "UpdateAdmin to zero should succeed: {:?}",
        result
    );

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
    let r = try_percolator_admin_ix_2(
        &mut env,
        &anyone,
        encode_set_oracle_authority(&anyone.pubkey()),
    );
    assert!(r.is_err(), "SetOracleAuthority must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(&mut env, &anyone, encode_resolve_market());
    assert!(r.is_err(), "ResolveMarket must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_6(&mut env, &anyone, encode_withdraw_insurance());
    assert!(r.is_err(), "WithdrawInsurance must fail after admin burn");

    env.advance_blockhash();
    let r = try_percolator_admin_ix_8(&mut env, &anyone, encode_admin_force_close(0));
    assert!(
        r.is_err(),
        "AdminForceCloseAccount must fail after admin burn"
    );

    env.advance_blockhash();
    let r = try_percolator_admin_ix_2(
        &mut env,
        &anyone,
        encode_set_insurance_withdraw_policy(&anyone.pubkey(), 1_000_000, 5000, 100),
    );
    assert!(
        r.is_err(),
        "SetInsuranceWithdrawPolicy must fail after admin burn"
    );

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
fn test_insurance_topup_authority_gated_withdraw_restricted() {
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
        ],
        data: encode_topup_insurance(1_000_000),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&donor.pubkey()),
        &[&donor],
        env.svm.latest_blockhash(),
    );
    assert!(
        env.svm.send_transaction(tx).is_err(),
        "non-authority insurance topup must be rejected"
    );
    assert_eq!(env.percolator_insurance_balance(&env.slab), 0);

    let slab = env.slab;
    env.topup_percolator_insurance(&slab, 1_000_000);

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
    env.svm
        .set_account(
            fake_mint,
            Account {
                lamports: 1_000_000,
                data: make_mint_data_with_authority(&fake_mint_auth),
                owner: fake_owner, // NOT spl_token::ID
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let result = env.try_init_coin_config_with_mint(&fake_mint);
    assert!(result.is_err(), "Non-SPL-owned mint must be rejected");
}

// ============================================================================
// Genesis -> DAO Squads handover (tags 32/33 via governance adapter tags 17/18)
// ============================================================================

fn squads_v4_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/squads_v4.so");
    assert!(
        path.exists(),
        "squads_v4.so missing at {:?}. Dump it: solana program dump -u m \
         SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf {:?}",
        path,
        path
    );
    path
}

fn squads_program_id() -> Pubkey {
    use std::str::FromStr;
    Pubkey::from_str("SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf").unwrap()
}

const SQUADS_PROGRAM_CONFIG_DISC: [u8; 8] = [196, 210, 90, 231, 144, 149, 140, 63];
const SQUADS_TIMELOCK_48H: u32 = 48 * 60 * 60;

/// Load Squads v4 and craft a fee-0 ProgramConfig at the canonical PDA.
/// Returns (program_config, treasury).
fn install_squads(env: &mut TestEnv) -> (Pubkey, Pubkey) {
    let squads = squads_program_id();
    let bytes = std::fs::read(squads_v4_path()).expect("read squads_v4.so");
    env.svm.add_program(squads, &bytes);

    let treasury = Pubkey::new_unique();
    env.svm
        .set_account(
            treasury,
            Account {
                lamports: 1_000_000_000,
                data: vec![],
                owner: solana_sdk::system_program::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let (program_config, _) =
        Pubkey::find_program_address(&[b"multisig", b"program_config"], &squads);
    let mut pc = vec![0u8; 144];
    pc[0..8].copy_from_slice(&SQUADS_PROGRAM_CONFIG_DISC);
    // authority@8 unused for create; fee@40 = 0; treasury@48.
    pc[48..80].copy_from_slice(treasury.as_ref());
    env.svm
        .set_account(
            program_config,
            Account {
                lamports: 10_000_000,
                data: pc,
                owner: squads,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    (program_config, treasury)
}

/// (create_key PDA of the rewards program, derived Squads multisig address).
fn squads_multisig_for(env: &TestEnv) -> (Pubkey, Pubkey) {
    let squads = squads_program_id();
    let (create_key, _) = Pubkey::find_program_address(
        &[b"genesis_squads", env.coin_mint.as_ref()],
        &env.rewards_id,
    );
    let (multisig, _) =
        Pubkey::find_program_address(&[b"multisig", b"multisig", create_key.as_ref()], &squads);
    (create_key, multisig)
}

fn multisig_config_authority(data: &[u8]) -> Pubkey {
    Pubkey::new_from_array(data[40..72].try_into().unwrap())
}

/// The genesis market is born under a program-created Squads 1/1 + 48h multisig,
/// and control is handed to the winning DAO by rotating config_authority — all
/// driven through governance -> rewards -> Squads CPIs against the real binary.
#[test]
fn test_genesis_squads_create_and_handover_through_governance() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let (program_config, treasury) = install_squads(&mut env);
    let squads = squads_program_id();
    let (create_key, multisig) = squads_multisig_for(&env);
    let market_admin = env.market_admin_pda();
    let signer = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();

    // --- governance tag 17: create the controlled 1/1 + 48h multisig ---
    let create_ix = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(signer.pubkey(), true),                        // payer/creator
            AccountMeta::new_readonly(env.governance_authority_pda, false), // authority
            AccountMeta::new_readonly(env.rewards_id, false),               // rewards_program
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new_readonly(market_admin, false),
            AccountMeta::new_readonly(create_key, false),
            AccountMeta::new_readonly(squads, false),
            AccountMeta::new_readonly(program_config, false),
            AccountMeta::new(treasury, false),
            AccountMeta::new(multisig, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![17u8],
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            create_ix,
        ],
        Some(&signer.pubkey()),
        &[&signer],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .expect("init_genesis_squads via governance failed");

    let ms = env.svm.get_account(&multisig).expect("multisig created");
    assert_eq!(ms.owner, squads, "multisig owned by Squads");
    assert_eq!(
        multisig_config_authority(&ms.data),
        market_admin,
        "config_authority is the program's market_admin PDA at genesis",
    );
    assert_eq!(
        u16::from_le_bytes(ms.data[72..74].try_into().unwrap()),
        1,
        "1/1 threshold",
    );
    assert_eq!(
        u32::from_le_bytes(ms.data[74..78].try_into().unwrap()),
        SQUADS_TIMELOCK_48H,
        "48h timelock",
    );

    // --- craft a finalized GenesisConfig so handover is permitted ---
    let genesis_cfg = env.genesis_cfg_pda();
    let mut gdata = vec![0u8; 160]; // GenesisConfig size
    gdata[0..8].copy_from_slice(b"GENCFG01");
    gdata[8..40].copy_from_slice(env.coin_mint.as_ref()); // coin_mint
    gdata[136] = 1; // finalized
    gdata[137] = 1; // kicked
    env.svm
        .set_account(
            genesis_cfg,
            Account {
                lamports: 10_000_000,
                data: gdata,
                owner: env.rewards_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // --- governance tag 18: rotate config_authority -> winning DAO ---
    let winning_dao = Pubkey::new_unique();
    let handover_ix = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(signer.pubkey(), true),
            AccountMeta::new_readonly(env.governance_authority_pda, false),
            AccountMeta::new_readonly(env.rewards_id, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new_readonly(genesis_cfg, false),
            AccountMeta::new_readonly(market_admin, false),
            AccountMeta::new_readonly(squads, false),
            AccountMeta::new(multisig, false),
            AccountMeta::new_readonly(winning_dao, false),
        ],
        data: vec![18u8],
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            handover_ix,
        ],
        Some(&signer.pubkey()),
        &[&signer],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .expect("handover_genesis_squads via governance failed");

    let ms = env.svm.get_account(&multisig).unwrap();
    assert_eq!(
        multisig_config_authority(&ms.data),
        winning_dao,
        "config_authority handed to the winning DAO",
    );
    assert_eq!(
        u32::from_le_bytes(ms.data[74..78].try_into().unwrap()),
        SQUADS_TIMELOCK_48H,
        "48h timelock preserved across handover",
    );
}

/// Handover must be rejected while genesis is not finalized.
#[test]
fn test_genesis_squads_handover_requires_finalized_genesis() {
    let mut env = TestEnv::new();
    env.init_coin_config();
    let (program_config, treasury) = install_squads(&mut env);
    let squads = squads_program_id();
    let (create_key, multisig) = squads_multisig_for(&env);
    let market_admin = env.market_admin_pda();
    let signer = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();

    let create_ix = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(signer.pubkey(), true),
            AccountMeta::new_readonly(env.governance_authority_pda, false),
            AccountMeta::new_readonly(env.rewards_id, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new_readonly(market_admin, false),
            AccountMeta::new_readonly(create_key, false),
            AccountMeta::new_readonly(squads, false),
            AccountMeta::new_readonly(program_config, false),
            AccountMeta::new(treasury, false),
            AccountMeta::new(multisig, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![17u8],
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            create_ix,
        ],
        Some(&signer.pubkey()),
        &[&signer],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("create failed");

    // Finalized=0 GenesisConfig -> handover must fail.
    let genesis_cfg = env.genesis_cfg_pda();
    let mut gdata = vec![0u8; 160]; // GenesisConfig size
    gdata[0..8].copy_from_slice(b"GENCFG01");
    gdata[8..40].copy_from_slice(env.coin_mint.as_ref());
    gdata[136] = 0; // NOT finalized
    gdata[137] = 1;
    env.svm
        .set_account(
            genesis_cfg,
            Account {
                lamports: 10_000_000,
                data: gdata,
                owner: env.rewards_id,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let winning_dao = Pubkey::new_unique();
    let handover_ix = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(signer.pubkey(), true),
            AccountMeta::new_readonly(env.governance_authority_pda, false),
            AccountMeta::new_readonly(env.rewards_id, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new_readonly(genesis_cfg, false),
            AccountMeta::new_readonly(market_admin, false),
            AccountMeta::new_readonly(squads, false),
            AccountMeta::new(multisig, false),
            AccountMeta::new_readonly(winning_dao, false),
        ],
        data: vec![18u8],
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            handover_ix,
        ],
        Some(&signer.pubkey()),
        &[&signer],
        env.svm.latest_blockhash(),
    );
    assert!(
        env.svm.send_transaction(tx).is_err(),
        "handover before finalization must fail",
    );
    // config_authority unchanged.
    let ms = env.svm.get_account(&multisig).unwrap();
    assert_eq!(multisig_config_authority(&ms.data), market_admin);
}

// ============================================================================
// Genesis bootstrap exit: withdraw + forfeit vote any time before voting starts
// ============================================================================

fn encode_genesis_bootstrap_withdraw(
    backing_domain: u8,
    insurance_pull: u64,
    backing_pull: u64,
) -> Vec<u8> {
    let mut d = vec![23u8, backing_domain];
    d.extend_from_slice(&insurance_pull.to_le_bytes());
    d.extend_from_slice(&backing_pull.to_le_bytes());
    d
}

/// Reads the position's start_slot (offset 56); 0 means never-deposited or exited.
fn read_genesis_start_slot(env: &TestEnv, user: &Pubkey) -> u64 {
    let pos = env.svm.get_account(&env.genesis_position_pda(user)).unwrap();
    u64::from_le_bytes(pos.data[56..64].try_into().unwrap())
}

fn read_genesis_position_amount(env: &TestEnv, user: &Pubkey) -> u64 {
    let pos = env.svm.get_account(&env.genesis_position_pda(user)).unwrap();
    u64::from_le_bytes(pos.data[40..48].try_into().unwrap())
}

/// Before kickstart the deposit is still in the genesis vault: a depositor gets a
/// full refund, forfeits their vote, and the pool shrinks.
#[test]
fn test_genesis_bootstrap_withdraw_before_kickstart_full_refund() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 5);
    assert!(read_genesis_start_slot(&env, &alice.pubkey()) > 0, "start slot recorded");
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 5);

    let collateral_mint = env.collateral_mint;
    let user_ata = env.create_ata(&collateral_mint, &alice.pubkey(), 0);
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new(env.genesis_cfg_pda(), false),
            AccountMeta::new(env.genesis_position_pda(&alice.pubkey()), false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(env.genesis_vault_pda(), false),
            AccountMeta::new_readonly(env.market_admin_pda(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: encode_genesis_bootstrap_withdraw(0, 0, 0),
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&alice.pubkey()),
        &[&alice],
        env.svm.latest_blockhash(),
    );
    env.svm
        .send_transaction(tx)
        .expect("bootstrap withdraw before kickstart failed");

    assert_eq!(env.read_token_balance(&user_ata), 5, "full refund");
    assert_eq!(read_genesis_start_slot(&env, &alice.pubkey()), 0, "vote forfeited");
    assert_eq!(read_genesis_position_amount(&env, &alice.pubkey()), 0, "principal claim cleared");
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 0, "vault drained");
    // total_deposited shrank to 0.
    let cfg = env.svm.get_account(&env.genesis_cfg_pda()).unwrap();
    assert_eq!(u64::from_le_bytes(cfg.data[104..112].try_into().unwrap()), 0);
}

/// After kickstart the deposit is deployed into the market. A depositor exits by
/// withdrawing a pro-rata share of the shared vault — withdraw itself never touches
/// the market, so no exit can drain it first-come-first-served. Futarchy recovers
/// market capital into the shared vault (it stays at risk until then); on a healthy,
/// fully-recovered market the depositor recovers their full principal.
#[test]
fn test_genesis_bootstrap_exit_after_kickstart_recovers_then_withdraws() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 2);
    env.genesis_deposit(&bob, 2);

    let (slab, percolator_vault) = env.init_futarchy_percolator_market();
    env.kickstart_genesis_market(&slab, &percolator_vault);
    assert_eq!(env.percolator_insurance_balance(&slab), 2);
    assert_eq!(env.read_token_balance(&percolator_vault), 4);
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 0);

    // A direct market pull from withdraw is now rejected (this was the bank-run path).
    let bad = genesis_bootstrap_exit_ix(&mut env, &alice, 0, 1, 1, Some((slab, percolator_vault)));
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), bad],
        Some(&alice.pubkey()),
        &[&alice],
        env.svm.latest_blockhash(),
    );
    assert!(env.svm.send_transaction(tx).is_err(), "withdraw no longer pulls from the market");

    // Recovery is futarchy-gated and requires the COIN to be live.
    env.set_clock(150);
    env.activate_live();
    env.governance_recover(&slab, &percolator_vault, 0, 0, 2)
        .expect("governance insurance recovery");
    env.governance_recover(&slab, &percolator_vault, 1, 0, 2)
        .expect("governance backing recovery");
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 4, "market gathered into shared vault");

    // Alice withdraws her pro-rata share of the (fully recovered, healthy) vault:
    // her full principal of 2.
    let user_ata = env.genesis_withdraw(&alice);
    assert_eq!(env.read_token_balance(&user_ata), 2, "full principal recovered on a healthy market");
    assert_eq!(read_genesis_start_slot(&env, &alice.pubkey()), 0, "vote forfeited");
}

/// A depositor who never voted may exit at any time, including during voting —
/// there is no blanket lock. Their capital leaves the pool (the quorum electorate
/// shrinks); only a *voter* must retract first (see the meta-only test below).
#[test]
fn test_genesis_nonvoter_exits_freely_during_voting() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 3);

    // Warp past the bootstrap delay and go live (voting starts).
    env.svm.set_sysvar(&Clock {
        slot: 200,
        unix_timestamp: 200,
        ..Clock::default()
    });
    env.activate_live();

    let collateral_mint = env.collateral_mint;
    let user_ata = env.create_ata(&collateral_mint, &alice.pubkey(), 0);
    let ix = Instruction {
        program_id: env.rewards_id,
        accounts: vec![
            AccountMeta::new(alice.pubkey(), true),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new(env.genesis_cfg_pda(), false),
            AccountMeta::new(env.genesis_position_pda(&alice.pubkey()), false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(env.genesis_vault_pda(), false),
            AccountMeta::new_readonly(env.market_admin_pda(), false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: encode_genesis_bootstrap_withdraw(0, 0, 0),
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&alice.pubkey()),
        &[&alice],
        env.svm.latest_blockhash(),
    );
    assert!(
        env.svm.send_transaction(tx).is_ok(),
        "a non-voter may exit during voting"
    );
    assert_eq!(
        read_genesis_position_amount(&env, &alice.pubkey()),
        0,
        "principal fully refunded on exit"
    );
    assert_eq!(
        env.read_token_balance(&user_ata),
        3,
        "got the full deposit back"
    );
}

/// During voting a depositor who has voted must retract every ballot before
/// exiting. Retraction backs their weight + principal out of the tally; once they
/// withdraw, the quorum denominator shrinks too — quorum is recomputed as people
/// leave, and a ballot can never outlive the capital that backed it.
#[test]
fn test_voter_retracts_then_exits_and_quorum_recomputes() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(1);
    env.init_genesis_bootstrap(100);

    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 3); // slot 100
    env.genesis_deposit(&bob, 1);
    env.set_clock(120); // live (delay 1), age >= 2 for nonzero weight
    env.activate_live();

    let dest = env.create_coin_ata(&alice.pubkey(), 0);
    env.init_genesis_distribution(1, &dest);
    env.vote_genesis_distribution(&alice, 1);
    env.vote_genesis_distribution(&bob, 1);

    // The proposal's support_principal (data[88..96]) is the sum of every backer's
    // raw principal.
    let support_principal = |env: &TestEnv| {
        let p = env.svm.get_account(&env.genesis_distribution_pda(1)).unwrap();
        u64::from_le_bytes(p.data[88..96].try_into().unwrap())
    };
    assert_eq!(support_principal(&env), 4, "both ballots counted (3 + 1)");

    // Alice has a live ballot, so she cannot exit yet.
    assert!(
        env.try_genesis_withdraw(&alice).is_err(),
        "a voter cannot exit while a ballot is live"
    );

    // She retracts (gives up the vote): her principal leaves the quorum tally.
    env.retract_genesis_vote(&alice, 1).expect("retract");
    assert_eq!(
        support_principal(&env),
        1,
        "alice's principal is backed out of the quorum tally on retract"
    );
    // Her ballot on the position is cleared too.
    assert_eq!(
        read_position_vote_weight(&env, &alice.pubkey()),
        0,
        "retracted ballot leaves no weight on the position"
    );

    // Double-retract is rejected (nothing left to give up).
    assert!(
        env.retract_genesis_vote(&alice, 1).is_err(),
        "cannot retract an already-retracted ballot"
    );

    // Now she can exit; her capital leaves the pool (pre-kickstart full refund),
    // so outstanding principal — and the quorum denominator — shrinks to bob's 1.
    let alice_ata = env.genesis_withdraw(&alice);
    assert_eq!(env.read_token_balance(&alice_ata), 3, "full refund on exit");
    assert_eq!(
        read_genesis_position_amount(&env, &alice.pubkey()),
        0,
        "position emptied"
    );
}

// ============================================================================
// Full genesis -> DAO lifecycle, end to end, against the real percolator,
// governance, rewards, and Squads v4 binaries in LiteSVM.
// ============================================================================

/// One continuous run exercising every phase with no test shortcuts:
/// deposit -> create real market -> create Squads 1/1+48h -> real 50/50 kickstart
/// (with capital-protected insurance policy) -> go live -> a depositor exits
/// (futarchy recovers market principal into the shared vault, then she takes a
/// pro-rata withdraw) -> propose/vote/mint 100% of supply -> finalize -> hand the
/// Squads config-authority to the winning DAO -> remaining depositors withdraw.
#[test]
fn test_full_genesis_to_dao_lifecycle_end_to_end() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);
    let (program_config, treasury) = install_squads(&mut env);
    let squads = squads_program_id();
    let (create_key, multisig) = squads_multisig_for(&env);
    let market_admin = env.market_admin_pda();
    let dao_signer = Keypair::from_bytes(&env.dao_authority.to_bytes()).unwrap();

    // --- 1. Deposits during the window (1 base unit = 1 vote) ---
    let alice = Keypair::new();
    let bob = Keypair::new();
    let carol = Keypair::new();
    for kp in [&alice, &bob, &carol] {
        env.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }
    env.genesis_deposit(&alice, 4);
    env.genesis_deposit(&bob, 4);
    env.genesis_deposit(&carol, 2);
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 10);

    // --- 2. Real Percolator market, born under the futarchy market_admin PDA ---
    let (slab, percolator_vault) = env.init_futarchy_percolator_market();

    // --- 3. Program creates the controlled 1/1 + 48h Squads multisig ---
    let create_squads = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(dao_signer.pubkey(), true),
            AccountMeta::new_readonly(env.governance_authority_pda, false),
            AccountMeta::new_readonly(env.rewards_id, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new_readonly(market_admin, false),
            AccountMeta::new_readonly(create_key, false),
            AccountMeta::new_readonly(squads, false),
            AccountMeta::new_readonly(program_config, false),
            AccountMeta::new(treasury, false),
            AccountMeta::new(multisig, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ],
        data: vec![17u8],
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), create_squads],
        Some(&dao_signer.pubkey()),
        &[&dao_signer],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("create squads multisig");
    let ms = env.svm.get_account(&multisig).unwrap();
    assert_eq!(multisig_config_authority(&ms.data), market_admin, "born under market_admin");

    // --- 4. Real 50/50 kickstart (also sets the capital-protected insurance policy) ---
    env.kickstart_genesis_market(&slab, &percolator_vault);
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 0, "vault deployed to market");
    assert_eq!(env.percolator_insurance_balance(&slab), 5, "half to insurance");
    assert_eq!(env.read_token_balance(&percolator_vault), 10);

    // --- 5. Go live (voting opens; futarchy recovery becomes available) ---
    env.set_clock(150);
    env.activate_live();

    // --- 6. Carol exits. Withdraw never pulls from the market; futarchy recovers the
    // healthy market principal back into the shared vault, then Carol claims her
    // pro-rata share — her full 2. (This is the post-fix replacement for the old
    // unilateral market pull that allowed the first-come-first-served bank run.) ---
    env.governance_recover(&slab, &percolator_vault, 0, 0, 5)
        .expect("governance insurance recovery");
    env.governance_recover(&slab, &percolator_vault, 1, 0, 5)
        .expect("governance backing recovery");
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 10, "healthy market gathered into shared vault");
    let carol_ata = env.genesis_withdraw(&carol);
    assert_eq!(env.read_token_balance(&carol_ata), 2, "carol recovers her full principal (healthy market)");
    assert_eq!(read_genesis_start_slot(&env, &carol.pubkey()), 0, "carol forfeited her vote");
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 8, "carol paid; 8 remains for alice+bob");

    // --- 7. One proposal wins; the permissionless trigger mints 100% of supply ---
    let dao_coin = env.create_coin_ata(&alice.pubkey(), 0);
    let bob_coin = env.create_coin_ata(&bob.pubkey(), 0);
    env.init_genesis_distribution(1, &dao_coin);
    env.init_genesis_distribution(2, &bob_coin);
    // Remaining depositors (carol exited) back proposal 1, giving it the full cast
    // weight and a principal quorum; anyone may then crank it.
    env.vote_genesis_distribution(&alice, 1);
    env.vote_genesis_distribution(&bob, 1);
    let cranker = Keypair::new();
    env.svm.airdrop(&cranker.pubkey(), 10_000_000_000).unwrap();
    env.trigger_genesis_distribution(&cranker, 1, &dao_coin);
    assert_eq!(env.read_token_balance(&dao_coin), 100, "winner-take-all: full supply minted");
    assert_eq!(env.read_token_balance(&bob_coin), 0, "the losing proposal mints nothing");

    // --- 8. Market principal is already in the shared vault (cranked at step 5),
    // so there is nothing left to recover pre-finalize. ---
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 8, "alice+bob principal sits in the vault");

    // --- 9. Finalize ---
    env.finalize_genesis();
    let cfg = env.svm.get_account(&env.genesis_cfg_pda()).unwrap();
    assert_eq!(cfg.data[136], 1, "finalized");

    // --- 10. Hand the Squads config-authority to the winning DAO ---
    let winning_dao = Pubkey::new_unique();
    let handover = Instruction {
        program_id: env.governance_id,
        accounts: vec![
            AccountMeta::new(dao_signer.pubkey(), true),
            AccountMeta::new_readonly(env.governance_authority_pda, false),
            AccountMeta::new_readonly(env.rewards_id, false),
            AccountMeta::new_readonly(env.coin_mint, false),
            AccountMeta::new_readonly(env.coin_cfg_pda(), false),
            AccountMeta::new_readonly(env.genesis_cfg_pda(), false),
            AccountMeta::new_readonly(market_admin, false),
            AccountMeta::new_readonly(squads, false),
            AccountMeta::new(multisig, false),
            AccountMeta::new_readonly(winning_dao, false),
        ],
        data: vec![18u8],
    };
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), handover],
        Some(&dao_signer.pubkey()),
        &[&dao_signer],
        env.svm.latest_blockhash(),
    );
    env.svm.send_transaction(tx).expect("squads handover to DAO");
    let ms = env.svm.get_account(&multisig).unwrap();
    assert_eq!(multisig_config_authority(&ms.data), winning_dao, "DAO controls the multisig");

    // --- 11. Remaining depositors withdraw their principal from the refunded vault ---
    let alice_base = env.genesis_withdraw(&alice);
    let bob_base = env.genesis_withdraw(&bob);
    assert_eq!(env.read_token_balance(&alice_base), 4, "alice principal returned");
    assert_eq!(env.read_token_balance(&bob_base), 4, "bob principal returned");
    assert_eq!(read_genesis_start_slot(&env, &alice.pubkey()), 0, "votes worthless after withdrawal");
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 0, "vault fully distributed");
}

// ============================================================================
// Time-weighted vote coverage + bootstrap-exit input guards (restored)
// ============================================================================

/// A proposal's cumulative support weight (sum of every backer's
/// floor(log2(hold)) * principal), at GenesisDistribution.data[80..88].
fn read_proposal_support_weight(env: &TestEnv, proposal_id: u64) -> u64 {
    let proposal = env.genesis_distribution_pda(proposal_id);
    let rec = env.svm.get_account(&proposal).unwrap();
    u64::from_le_bytes(rec.data[80..88].try_into().unwrap())
}

/// A single position's recorded ballot weight, read off its GenesisPosition at
/// voted_weight (data[96..104]). Zero once the voter has retracted or exited.
fn read_position_vote_weight(env: &TestEnv, voter: &Pubkey) -> u64 {
    let pos = env.svm.get_account(&env.genesis_position_pda(voter)).unwrap();
    u64::from_le_bytes(pos.data[96..104].try_into().unwrap())
}

/// Two equal-size deposits, one earlier: the earlier joiner weighs strictly more.
#[test]
fn test_genesis_vote_weight_rewards_earlier_joiners() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(2000);
    env.init_genesis_bootstrap(100);

    let early = Keypair::new();
    let late = Keypair::new();
    for u in [&early, &late] {
        env.svm.airdrop(&u.pubkey(), 10_000_000_000).unwrap();
    }
    env.genesis_deposit(&early, 1); // slot 100 -> start_slot 100
    env.set_clock(2000);
    env.genesis_deposit(&late, 1); // slot 2000 -> start_slot 2000
    env.set_clock(2100);
    env.activate_live();

    let dest = env.create_coin_ata(&early.pubkey(), 0);
    env.init_genesis_distribution(1, &dest);
    // Both back the same proposal; each backer's contribution is recorded on its
    // own position (voted_weight) and accumulated into the proposal.
    env.vote_genesis_distribution(&early, 1);
    env.vote_genesis_distribution(&late, 1);

    let early_w = read_position_vote_weight(&env, &early.pubkey());
    let late_w = read_position_vote_weight(&env, &late.pubkey());
    assert_eq!(early_w, 10, "floor(log2(2000)) * 1");
    assert_eq!(late_w, 6, "floor(log2(100)) * 1");
    assert!(early_w > late_w, "the earlier joiner weighs strictly more per unit principal");
    // Proposal support is the sum of both backers' weights.
    assert_eq!(read_proposal_support_weight(&env, 1), early_w + late_w);
}

/// A second deposit resets the start slot (last-write-time): topping up late
/// surrenders the early-join multiplier even as principal grows.
#[test]
fn test_genesis_last_deposit_resets_vote_clock() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(2000);
    env.init_genesis_bootstrap(100);

    let steady = Keypair::new();
    let topper = Keypair::new();
    for u in [&steady, &topper] {
        env.svm.airdrop(&u.pubkey(), 10_000_000_000).unwrap();
    }
    env.genesis_deposit(&steady, 1); // start_slot 100, untouched
    env.genesis_deposit(&topper, 1); // start_slot 100
    env.set_clock(2000);
    env.genesis_deposit(&topper, 1); // last-write-time resets start_slot -> 2000
    env.set_clock(2100);
    env.activate_live();

    let dest = env.create_coin_ata(&steady.pubkey(), 0);
    env.init_genesis_distribution(1, &dest);
    env.vote_genesis_distribution(&steady, 1);
    env.vote_genesis_distribution(&topper, 1);

    // steady: age 2000, staked 1 -> floor(log2(2000)) * 1 = 10.
    // topper: reset to slot 2000 so age 100, staked 2 -> floor(log2(100)) * 2 = 12.
    assert_eq!(read_position_vote_weight(&env, &steady.pubkey()), 10);
    assert_eq!(read_position_vote_weight(&env, &topper.pubkey()), 12);
}

/// Approval needs more than half the outstanding principal to have voted; exactly
/// half is not a quorum.
#[test]
fn test_genesis_distribution_requires_principal_quorum() {
    let mut env = TestEnv::new_meta_only();
    env.init_coin_config_with_delay(2000);
    env.init_genesis_bootstrap(100);

    let a = Keypair::new();
    let b = Keypair::new();
    let c = Keypair::new();
    for k in [&a, &b, &c] {
        env.svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
    }
    env.genesis_deposit(&a, 2);
    env.genesis_deposit(&b, 1);
    env.genesis_deposit(&c, 1); // outstanding principal = 4, quorum needs > 2
    env.set_clock(2100);
    env.activate_live();
    env.force_genesis_kicked_for_test(); // mint now requires a kicked market

    let dest = env.create_coin_ata(&a.pubkey(), 0);
    env.init_genesis_distribution(1, &dest);

    let cranker = Keypair::new();
    env.svm.airdrop(&cranker.pubkey(), 10_000_000_000).unwrap();

    env.vote_genesis_distribution(&a, 1); // voted principal 2 = exactly half of 4
    assert!(
        env.try_trigger_genesis_distribution(&cranker, 1, &dest).is_err(),
        "exactly half the outstanding principal is not a quorum"
    );
    env.vote_genesis_distribution(&b, 1); // voted principal -> 3, and 3*2 > 4
    // Permissionless: a fresh funded keypair (not governance) fires the trigger,
    // minting the full reward_supply (100) winner-take-all.
    env.trigger_genesis_distribution(&cranker, 1, &dest);
    assert_eq!(env.read_token_balance(&dest), 100);
}

fn genesis_bootstrap_exit_ix(
    env: &mut TestEnv,
    user: &Keypair,
    backing_domain: u8,
    insurance_pull: u64,
    backing_pull: u64,
    market: Option<(Pubkey, Pubkey)>,
) -> Instruction {
    let collateral_mint = env.collateral_mint;
    let user_ata = env.create_ata(&collateral_mint, &user.pubkey(), 0);
    let mut accounts = vec![
        AccountMeta::new(user.pubkey(), true),
        AccountMeta::new_readonly(env.coin_mint, false),
        AccountMeta::new_readonly(env.coin_cfg_pda(), false),
        AccountMeta::new(env.genesis_cfg_pda(), false),
        AccountMeta::new(env.genesis_position_pda(&user.pubkey()), false),
        AccountMeta::new(user_ata, false),
        AccountMeta::new(env.genesis_vault_pda(), false),
        AccountMeta::new_readonly(env.market_admin_pda(), false),
        AccountMeta::new_readonly(spl_token::ID, false),
    ];
    if let Some((slab, percolator_vault)) = market {
        let (pvp, _) = Pubkey::find_program_address(&[b"vault", slab.as_ref()], &env.percolator_id);
        accounts.push(AccountMeta::new(slab, false));
        accounts.push(AccountMeta::new(percolator_vault, false));
        accounts.push(AccountMeta::new_readonly(pvp, false));
        accounts.push(AccountMeta::new_readonly(env.percolator_id, false));
    }
    Instruction {
        program_id: env.rewards_id,
        accounts,
        data: encode_genesis_bootstrap_withdraw(backing_domain, insurance_pull, backing_pull),
    }
}

/// Before kickstart there is no market to draw from, so market-pull amounts are rejected.
#[test]
fn test_genesis_bootstrap_exit_prekickstart_rejects_market_pull() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);
    let alice = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 4);

    let ix = genesis_bootstrap_exit_ix(&mut env, &alice, 0, 1, 0, None);
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&alice.pubkey()),
        &[&alice],
        env.svm.latest_blockhash(),
    );
    assert!(
        env.svm.send_transaction(tx).is_err(),
        "pre-kickstart exit must reject market-pull amounts"
    );
    assert_eq!(read_genesis_position_amount(&env, &alice.pubkey()), 4);
    assert!(read_genesis_start_slot(&env, &alice.pubkey()) > 0);
}

/// A depositor cannot pull more than their own remaining principal from the shared pools.
#[test]
fn test_genesis_bootstrap_exit_rejects_overpull() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);
    let alice = Keypair::new();
    let bob = Keypair::new();
    env.svm.airdrop(&alice.pubkey(), 10_000_000_000).unwrap();
    env.svm.airdrop(&bob.pubkey(), 10_000_000_000).unwrap();
    env.genesis_deposit(&alice, 2);
    env.genesis_deposit(&bob, 2);
    let (slab, percolator_vault) = env.init_futarchy_percolator_market();
    env.kickstart_genesis_market(&slab, &percolator_vault);

    // alice's remaining is 2; pulling 2 + 2 = 4 exceeds it.
    let ix = genesis_bootstrap_exit_ix(&mut env, &alice, 0, 2, 2, Some((slab, percolator_vault)));
    env.svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ix,
        ],
        Some(&alice.pubkey()),
        &[&alice],
        env.svm.latest_blockhash(),
    );
    assert!(
        env.svm.send_transaction(tx).is_err(),
        "cannot recover more than the remaining principal"
    );
    assert_eq!(env.percolator_insurance_balance(&slab), 2, "market untouched on rejected over-pull");
}

/// FIX VERIFICATION — the former pre-finalization withdrawal race is gone.
///
/// Withdraw no longer pulls from the market; capital is brought back into the shared
/// genesis_vault by the (futarchy-gated) recover_genesis_market, and exits pay
/// a pro-rata share of that shared vault with the full claim settled. So on a
/// loss-bearing market every depositor recovers the same share regardless of order,
/// and looping pays nothing — the same guarantee
/// `test_underfunded_genesis_haircut_is_order_independent_and_loop_proof` proves for
/// the finalized path, now holding in the live decision window too. (Before the fix
/// this scenario paid the first mover 100% and the equal co-depositor 0.)
#[test]
fn test_prefinalization_exit_is_pro_rata_and_order_independent() {
    let mut env = TestEnv::new();
    env.init_coin_config_with_delay(50);
    env.init_genesis_bootstrap(100);

    // Two equal depositors: 2 base units each (total pool 4).
    let alice = Keypair::new();
    let bob = Keypair::new();
    for kp in [&alice, &bob] {
        env.svm.airdrop(&kp.pubkey(), 10_000_000_000).unwrap();
    }
    env.genesis_deposit(&alice, 2);
    env.genesis_deposit(&bob, 2);

    // Real Percolator market, kickstarted 50/50: collateral token vault = 4.
    let (slab, percolator_vault) = env.init_futarchy_percolator_market();
    env.kickstart_genesis_market(&slab, &percolator_vault);
    assert_eq!(env.read_token_balance(&percolator_vault), 4, "pool deployed to market");

    // The bootstrap market takes a FAITHFUL 50% trading loss: the insurance side
    // (2 units) is wiped — internal insurance counter, per-domain budgets, vault
    // counter, and the token vault all debited in lockstep. Backing (2) survives.
    // So 2 of the 4 deployed units are recoverable against 4 outstanding principal;
    // fair pro-rata = 1 each.
    env.simulate_insurance_loss(&slab, &percolator_vault, 2);

    // Live decision window: COIN live, market kicked, NOT finalized.
    env.set_clock(150);
    env.activate_live();

    // Recovery is bounded by Percolator's REAL post-loss accounting, not a hand-picked
    // amount: the wiped insurance can no longer be recovered (engine caps to 0).
    assert!(
        env.governance_recover(&slab, &percolator_vault, 0, 0, 1).is_err(),
        "engine caps recovery to the real post-loss insurance (0); the loss is not recoverable"
    );

    // Futarchy recovers the SURVIVING collateral (backing) into the SHARED vault.
    env.governance_recover(&slab, &percolator_vault, 1, 0, 2)
        .expect("governance recovery of the surviving backing");
    assert_eq!(env.read_token_balance(&env.genesis_vault_pda()), 2, "survivors gathered into shared vault");

    // Alice exits FIRST: pro-rata share of the shared vault = 2 * 2/4 = 1.
    let alice_ata = env.genesis_withdraw(&alice);
    assert_eq!(env.read_token_balance(&alice_ata), 1, "first mover gets her fair 50% share, not 100%");

    // Looping pays nothing more — the full claim is settled.
    let alice_again = env.genesis_withdraw(&alice);
    assert_eq!(env.read_token_balance(&alice_again), 0, "re-withdrawing yields nothing: no loop extraction");

    // Bob exits SECOND and recovers the SAME fair share — the straggler is NOT wiped
    // out. (Before the fix bob, going second against the drained market, got 0.)
    let bob_ata = env.genesis_withdraw(&bob);
    assert_eq!(env.read_token_balance(&bob_ata), 1, "the patient co-depositor recovers the same 50% share");
}
