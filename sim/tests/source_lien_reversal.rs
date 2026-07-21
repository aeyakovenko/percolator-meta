use litesvm::LiteSVM;
use percolator_prog::ix::Instruction as PIx;
use solana_sdk::{
    account::Account,
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};

const ATA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn perc_id() -> Pubkey {
    percolator_prog::id()
}

fn perc_so() -> String {
    format!(
        "{}/../target/deploy/percolator_prog.so",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn pix(accounts: Vec<AccountMeta>, instruction: PIx) -> Instruction {
    Instruction {
        program_id: perc_id(),
        accounts,
        data: instruction.encode(),
    }
}

fn token_account_data(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0; 165];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1;
    data
}

fn set_token(svm: &mut LiteSVM, key: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64) {
    svm.set_account(
        key,
        Account {
            lamports: 2_000_000,
            data: token_account_data(mint, owner, amount),
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn create_mint(svm: &mut LiteSVM, payer: &Keypair, authority: Pubkey) -> Pubkey {
    let mint = Keypair::new();
    let instructions = [
        solana_sdk::system_instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            svm.minimum_balance_for_rent_exemption(82),
            82,
            &spl_token::ID,
        ),
        spl_token::instruction::initialize_mint(
            &spl_token::ID,
            &mint.pubkey(),
            &authority,
            None,
            6,
        )
        .unwrap(),
    ];
    svm.send_transaction(Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer, &mint],
        svm.latest_blockhash(),
    ))
    .unwrap();
    mint.pubkey()
}

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    admin: Keypair,
    oracle: Keypair,
    provider: Keypair,
    market: Pubkey,
    collateral: Pubkey,
    vault: Pubkey,
    portfolio_len: usize,
}

impl Env {
    fn new() -> Self {
        const PRICE: u64 = 100;

        let mut svm = LiteSVM::new().with_compute_budget(
            solana_program_runtime::compute_budget::ComputeBudget {
                compute_unit_limit: 1_400_000,
                heap_size: 256 * 1024,
                ..solana_program_runtime::compute_budget::ComputeBudget::default()
            },
        );
        svm.add_program_from_file(perc_id(), perc_so()).unwrap();
        let payer = Keypair::new();
        let admin = Keypair::new();
        let oracle = Keypair::new();
        let provider = Keypair::new();
        for signer in [&payer, &admin, &oracle, &provider] {
            svm.airdrop(&signer.pubkey(), 100_000_000_000).unwrap();
        }
        svm.set_sysvar(&Clock {
            slot: 100,
            unix_timestamp: 100,
            ..Clock::default()
        });

        let collateral = create_mint(&mut svm, &payer, admin.pubkey());
        let market = Pubkey::new_unique();
        svm.set_account(
            market,
            Account {
                lamports: 1_000_000_000,
                data: vec![0; percolator_prog::state::market_account_len_for_capacity(1).unwrap()],
                owner: perc_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        let vault_authority =
            Pubkey::find_program_address(&[b"vault", market.as_ref()], &perc_id()).0;
        let vault = Pubkey::find_program_address(
            &[
                vault_authority.as_ref(),
                spl_token::ID.as_ref(),
                collateral.as_ref(),
            ],
            &ATA_PROGRAM_ID,
        )
        .0;
        set_token(&mut svm, vault, collateral, vault_authority, 0);

        let mut env = Self {
            svm,
            payer,
            admin,
            oracle,
            provider,
            market,
            collateral,
            vault,
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(1)
                .unwrap(),
        };
        let provider = env.provider.insecure_clone();
        env.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new_readonly(collateral, false),
                ],
                PIx::InitMarket {
                    max_portfolio_assets: 1,
                    h_min: 0,
                    h_max: 10,
                    initial_price: PRICE,
                    min_nonzero_mm_req: 1,
                    min_nonzero_im_req: 2,
                    maintenance_margin_bps: 1_000,
                    initial_margin_bps: 5_000,
                    max_trading_fee_bps: 10_000,
                    trade_fee_base_bps: 0,
                    liquidation_fee_bps: 0,
                    liquidation_fee_cap: 0,
                    min_liquidation_abs: 0,
                    max_price_move_bps_per_slot: 500,
                    max_accrual_dt_slots: 1,
                    max_abs_funding_e9_per_slot: 0,
                    min_funding_lifetime_slots: 1,
                    max_account_b_settlement_chunks: 1,
                    max_bankrupt_close_chunks: 1,
                    max_bankrupt_close_lifetime_slots: 100,
                    public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                    maintenance_fee_per_slot: 0,
                },
            ),
            &[&provider],
        )
        .expect("initialize a normal live market");
        env.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index: 0,
                    now_slot: 100,
                    initial_mark_e6: PRICE,
                },
            ),
            &[&provider],
        )
        .expect("configure the authenticated mark");

        let admin = env.admin.insecure_clone();
        env.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::UpdateAuthority {
                    new_pubkey: admin.pubkey().to_bytes(),
                },
            ),
            &[&provider, &admin],
        )
        .expect("remove the provider's global market authority");
        let oracle = env.oracle.insecure_clone();
        for (kind, replacement) in [
            (
                percolator_prog::processor::ASSET_AUTH_BACKING_BUCKET,
                provider.pubkey(),
            ),
            (
                percolator_prog::processor::ASSET_AUTH_ORACLE,
                oracle.pubkey(),
            ),
        ] {
            let replacement_signer = if replacement == provider.pubkey() {
                &provider
            } else {
                &oracle
            };
            env.send(
                pix(
                    vec![
                        AccountMeta::new(admin.pubkey(), true),
                        AccountMeta::new(replacement, true),
                        AccountMeta::new(market, false),
                    ],
                    PIx::UpdateAssetAuthority {
                        asset_index: 0,
                        kind,
                        new_pubkey: replacement.to_bytes(),
                    },
                ),
                &[&admin, replacement_signer],
            )
            .expect("separate the backing and oracle roles");
        }

        let market_data = env.svm.get_account(&market).unwrap().data;
        let (config, _) = percolator_prog::state::read_market(&market_data).unwrap();
        let profile = percolator_prog::state::read_asset_oracle_profile(&market_data, 0).unwrap();
        assert_eq!(config.marketauth, admin.pubkey().to_bytes());
        assert_eq!(profile.asset_admin, admin.pubkey().to_bytes());
        assert_eq!(profile.insurance_authority, admin.pubkey().to_bytes());
        assert_eq!(profile.insurance_operator, admin.pubkey().to_bytes());
        assert_eq!(
            profile.backing_bucket_authority,
            provider.pubkey().to_bytes(),
        );
        assert_eq!(profile.oracle_authority, env.oracle.pubkey().to_bytes());
        env
    }

    fn send(&mut self, instruction: Instruction, signers: &[&Keypair]) -> Result<(), String> {
        self.svm.expire_blockhash();
        let mut all_signers = vec![&self.payer];
        all_signers.extend_from_slice(signers);
        self.svm
            .send_transaction(Transaction::new_signed_with_payer(
                &[instruction],
                Some(&self.payer.pubkey()),
                &all_signers,
                self.svm.latest_blockhash(),
            ))
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
    }

    fn set_slot(&mut self, slot: u64) {
        self.svm.set_sysvar(&Clock {
            slot,
            unix_timestamp: slot as i64,
            ..Clock::default()
        });
    }

    fn create_portfolio(&mut self, owner: &Keypair, deposit: u64) -> Pubkey {
        self.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
        let portfolio = Pubkey::new_unique();
        self.svm
            .set_account(
                portfolio,
                Account {
                    lamports: 1_000_000_000,
                    data: vec![0; self.portfolio_len],
                    owner: perc_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        self.send(
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::InitPortfolio,
            ),
            &[owner],
        )
        .expect("initialize portfolio");
        if deposit != 0 {
            let source = Pubkey::new_unique();
            set_token(
                &mut self.svm,
                source,
                self.collateral,
                owner.pubkey(),
                deposit,
            );
            self.send(
                pix(
                    vec![
                        AccountMeta::new(owner.pubkey(), true),
                        AccountMeta::new(self.market, false),
                        AccountMeta::new(portfolio, false),
                        AccountMeta::new(source, false),
                        AccountMeta::new(self.vault, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    PIx::Deposit {
                        amount: u128::from(deposit),
                    },
                ),
                &[owner],
            )
            .expect("deposit portfolio capital");
        }
        portfolio
    }

    fn try_crank(&mut self, portfolio: Pubkey, slot: u64, observe: bool) -> Result<(), String> {
        let observations = if observe {
            vec![percolator_prog::ix::CrankObservationHint {
                asset_index: 0,
                oracle_accounts: 0,
            }]
        } else {
            vec![]
        };
        self.send(
            pix(
                vec![
                    AccountMeta::new(self.payer.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::PermissionlessCrank {
                    now_slot: slot,
                    observations,
                },
            ),
            &[],
        )
    }

    fn crank(&mut self, portfolio: Pubkey, slot: u64, observe: bool) {
        self.try_crank(portfolio, slot, observe)
            .expect("permissionless crank makes progress");
    }
}

fn basis_position_q(account: &percolator::PortfolioAccountV16Account) -> i128 {
    account
        .legs
        .iter()
        .filter_map(|leg| leg.try_to_runtime().ok())
        .find(|leg| leg.active && leg.asset_index == 0)
        .expect("asset-zero leg remains present")
        .basis_pos_q
}

fn try_reduce(
    env: &mut Env,
    owner: &Keypair,
    portfolio: Pubkey,
    reduce_q: u128,
) -> Result<(), String> {
    env.send(
        pix(
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(portfolio, false),
            ],
            PIx::RebalanceReduce {
                asset_index: 0,
                reduce_q,
            },
        ),
        &[owner],
    )
}

// A fresh source-credit lien must remain unwindable when a normal oracle reversal removes the
// positive claim it secured. Otherwise every public refresh, liquidation, and owner reduction path
// rejects while the open position and the provider's backing remain locked.
#[test]
fn fresh_source_lien_mark_reversal_must_preserve_a_public_exit_path() {
    const PRICE: u64 = 100;
    const BACKING: u64 = 100_000;
    const POSITION_Q: i128 = 1_000 * percolator::POS_SCALE as i128;
    const INCREASE_Q: i128 = 50 * percolator::POS_SCALE as i128;
    const LIENED_Q: i128 = POSITION_Q + INCREASE_Q;

    let mut env = Env::new();
    let provider = env.provider.insecure_clone();
    let provider_source = Pubkey::new_unique();
    set_token(
        &mut env.svm,
        provider_source,
        env.collateral,
        provider.pubkey(),
        BACKING,
    );
    env.send(
        pix(
            vec![
                AccountMeta::new(provider.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(provider_source, false),
                AccountMeta::new(env.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::TopUpBackingBucket {
                domain: 1,
                amount: u128::from(BACKING),
                expiry_slot: 10_000,
            },
        ),
        &[&provider],
    )
    .expect("independent provider funds fresh short-side source backing");

    let owner = Keypair::new();
    let counterparty_owner = Keypair::new();
    let keeper_owner = Keypair::new();
    let portfolio = env.create_portfolio(&owner, 52_501);
    let counterparty = env.create_portfolio(&counterparty_owner, 1_000_000);
    let keeper = env.create_portfolio(&keeper_owner, 0);
    env.send(
        pix(
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(counterparty_owner.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(portfolio, false),
                AccountMeta::new(counterparty, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: POSITION_Q,
                exec_price: PRICE,
                fee_bps: 0,
            },
        ),
        &[&owner, &counterparty_owner],
    )
    .expect("users open a normal balanced position");

    env.set_slot(101);
    let oracle = env.oracle.insecure_clone();
    env.send(
        pix(
            vec![
                AccountMeta::new(oracle.pubkey(), true),
                AccountMeta::new(env.market, false),
            ],
            PIx::PushAuthMark {
                asset_index: 0,
                now_slot: 101,
                mark_e6: 105,
            },
        ),
        &[&oracle],
    )
    .expect("honest oracle creates source-backed positive PnL");
    env.crank(keeper, 101, true);
    env.crank(counterparty, 101, false);
    env.crank(portfolio, 101, false);
    let credited =
        percolator_prog::state::read_portfolio(&env.svm.get_account(&portfolio).unwrap().data)
            .unwrap();
    assert_eq!(credited.pnl.get(), 5_000);

    env.send(
        pix(
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(counterparty_owner.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(portfolio, false),
                AccountMeta::new(counterparty, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: INCREASE_Q,
                exec_price: 105,
                fee_bps: 0,
            },
        ),
        &[&owner, &counterparty_owner],
    )
    .expect("risk increase creates a real source-credit lien");
    let liened =
        percolator_prog::state::read_portfolio(&env.svm.get_account(&portfolio).unwrap().data)
            .unwrap();
    assert_eq!(basis_position_q(&liened), LIENED_Q);
    assert!(
        liened.source_domains[0].source_claim_liened_num.get() > 0,
        "the public risk increase must reserve provider backing",
    );
    assert!(
        liened.source_domains[0]
            .source_lien_effective_reserved
            .get()
            > 0
    );

    env.set_slot(102);
    env.send(
        pix(
            vec![
                AccountMeta::new(oracle.pubkey(), true),
                AccountMeta::new(env.market, false),
            ],
            PIx::PushAuthMark {
                asset_index: 0,
                now_slot: 102,
                mark_e6: PRICE,
            },
        ),
        &[&oracle],
    )
    .expect("honest oracle reverses the source-backed gain");
    env.crank(keeper, 102, true);
    env.crank(counterparty, 102, false);

    let market_before = env.svm.get_account(&env.market).unwrap();
    let portfolio_before = env.svm.get_account(&portfolio).unwrap();
    let reversal = env.send(
        pix(
            vec![
                AccountMeta::new(env.payer.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(portfolio, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: 102,
                observations: vec![],
            },
        ),
        &[],
    );
    if reversal.is_err() {
        assert_eq!(env.svm.get_account(&env.market).unwrap(), market_before);
        assert_eq!(env.svm.get_account(&portfolio).unwrap(), portfolio_before);
    }

    let first_reduce = try_reduce(&mut env, &owner, portfolio, percolator::POS_SCALE);
    let mut successful_owner_paths = u64::from(reversal.is_ok()) + u64::from(first_reduce.is_ok());
    let mut keeper_progress = 0u64;
    for slot in 103..=118 {
        env.set_slot(slot);
        if env.try_crank(keeper, slot, false).is_ok() {
            keeper_progress += 1;
        }
        if env.try_crank(portfolio, slot, false).is_ok() {
            successful_owner_paths += 1;
        }
        if try_reduce(&mut env, &owner, portfolio, percolator::POS_SCALE).is_ok() {
            successful_owner_paths += 1;
        }
    }

    let final_state =
        percolator_prog::state::read_portfolio(&env.svm.get_account(&portfolio).unwrap().data)
            .unwrap();
    let remaining_lien = final_state.source_domains[0].source_claim_liened_num.get();
    assert!(
        successful_owner_paths > 0 && basis_position_q(&final_state) < LIENED_Q,
        "a fresh source lien trapped the owner after an ordinary mark reversal: initial_crank={reversal:?}, initial_reduce={first_reduce:?}, keeper_progress={keeper_progress}, successful_owner_paths={successful_owner_paths}, remaining_lien={remaining_lien}, pnl={}, capital={}, basis_q={}",
        final_state.pnl.get(),
        final_state.capital.get(),
        basis_position_q(&final_state),
    );
}
