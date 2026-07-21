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
        const PRICE: u64 = 1;

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
                    maintenance_margin_bps: 10_000,
                    initial_margin_bps: 10_000,
                    max_trading_fee_bps: 10_000,
                    trade_fee_base_bps: 0,
                    liquidation_fee_bps: 0,
                    liquidation_fee_cap: 0,
                    min_liquidation_abs: 0,
                    max_price_move_bps_per_slot: 10_000,
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

fn portfolio_state(env: &Env, portfolio: Pubkey) -> percolator::PortfolioAccountV16Account {
    percolator_prog::state::read_portfolio(&env.svm.get_account(&portfolio).unwrap().data).unwrap()
}

fn active_leg(
    account: &percolator::PortfolioAccountV16Account,
) -> Option<percolator::PortfolioLegV16> {
    account
        .legs
        .iter()
        .filter_map(|leg| leg.try_to_runtime().ok())
        .find(|leg| leg.active && leg.asset_index == 0)
}

fn settle_social_loss(env: &mut Env, portfolio: Pubkey, slot: u64) {
    for _ in 0..8 {
        let account = portfolio_state(env, portfolio);
        let leg = active_leg(&account).expect("winner remains exposed before owner reduction");
        let (_, group) =
            percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
                .unwrap();
        if !leg.b_stale && leg.b_snap == group.assets[0].b_long_num {
            return;
        }
        env.try_crank(portfolio, slot, false)
            .expect("public crank settles the winner's social-loss index");
    }
    panic!("winner did not settle social loss in bounded public calls");
}

// A second ordinary bankruptcy can drain effective OI exactly when the accumulated B-division
// remainder and canonical side dust carry one whole atom. That reset boundary must commit through
// bounded public calls; it may not roll back forever and leave the underwater account live.
#[test]
fn reset_remainder_carry_must_not_rollback_public_bankruptcy() {
    let mut env = Env::new();

    let l1o = Keypair::new();
    let l2o = Keypair::new();
    let l3o = Keypair::new();
    let l4o = Keypair::new();
    let l5o = Keypair::new();
    let s1o = Keypair::new();
    let s2o = Keypair::new();
    let s3o = Keypair::new();

    let l1 = env.create_portfolio(&l1o, 1_000);
    let l2 = env.create_portfolio(&l2o, 1_000);
    let l3 = env.create_portfolio(&l3o, 1_000);
    let l4 = env.create_portfolio(&l4o, 1_000);
    let l5 = env.create_portfolio(&l5o, 1_000);
    let s1 = env.create_portfolio(&s1o, 2);
    let s2 = env.create_portfolio(&s2o, 5);
    let s3 = env.create_portfolio(&s3o, 1_000);

    for (long_owner, long, short_owner, short, q) in [
        (&l1o, l1, &s1o, s1, 1_897_305),
        (&l2o, l2, &s1o, s1, 102_695),
        (&l2o, l2, &s2o, s2, 666_301),
        (&l3o, l3, &s2o, s2, 65_831),
        (&l4o, l4, &s2o, s2, 430_043),
        (&l4o, l4, &s3o, s3, 1_130_061),
        (&l5o, l5, &s3o, s3, 767_244),
    ] {
        env.send(
            pix(
                vec![
                    AccountMeta::new(long_owner.pubkey(), true),
                    AccountMeta::new(short_owner.pubkey(), true),
                    AccountMeta::new(env.market, false),
                    AccountMeta::new(long, false),
                    AccountMeta::new(short, false),
                ],
                PIx::TradeNoCpi {
                    asset_index: 0,
                    size_q: i128::from(q),
                    exec_price: 1,
                    fee_bps: 0,
                },
            ),
            &[long_owner, short_owner],
        )
        .expect("ordinary users open balanced interest");
    }

    let oracle = env.oracle.insecure_clone();
    for (slot, mark) in [(101, 2), (102, 3), (103, 4), (104, 5), (105, 6)] {
        env.set_slot(slot);
        env.send(
            pix(
                vec![
                    AccountMeta::new(oracle.pubkey(), true),
                    AccountMeta::new(env.market, false),
                ],
                PIx::PushAuthMark {
                    asset_index: 0,
                    now_slot: slot,
                    mark_e6: mark,
                },
            ),
            &[&oracle],
        )
        .expect("independent oracle advances the normal mark");
        env.crank(s3, slot, true);
    }

    for _ in 0..8 {
        if active_leg(&portfolio_state(&env, s1)).is_none() {
            break;
        }
        env.try_crank(s1, 105, false)
            .expect("public crank closes the first bankrupt short");
    }

    let (_, after_first_bankruptcy) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    assert_eq!(after_first_bankruptcy.mode, percolator::MarketModeV16::Live);
    assert!(active_leg(&portfolio_state(&env, s1)).is_none());
    assert_eq!(after_first_bankruptcy.assets[0].oi_eff_long_q, 3_059_480);
    assert_eq!(after_first_bankruptcy.assets[0].oi_eff_short_q, 3_059_480);
    assert_eq!(
        after_first_bankruptcy.assets[0].social_loss_remainder_long_num,
        322_760
    );
    assert_eq!(
        after_first_bankruptcy.assets[0].social_loss_dust_long_num,
        0
    );

    settle_social_loss(&mut env, l1, 105);
    let l1_leg = active_leg(&portfolio_state(&env, l1)).unwrap();
    assert_eq!(
        l1_leg.b_rem,
        percolator::SOCIAL_LOSS_DEN - 121_035,
        "the public setup must reach the reset carry boundary"
    );
    env.send(
        pix(
            vec![
                AccountMeta::new(l1o.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(l1, false),
            ],
            PIx::RebalanceReduce {
                asset_index: 0,
                reduce_q: 1_897_305,
            },
        ),
        &[&l1o],
    )
    .expect("the first winner exits normally");
    assert!(active_leg(&portfolio_state(&env, l1)).is_none());

    let (_, after_winner_exit) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    assert_eq!(after_winner_exit.assets[0].oi_eff_long_q, 1_162_175);
    assert_eq!(after_winner_exit.assets[0].oi_eff_short_q, 1_162_175);
    assert_eq!(
        after_winner_exit.assets[0].social_loss_remainder_long_num,
        322_760
    );
    assert_eq!(
        after_winner_exit.assets[0].social_loss_dust_long_num,
        percolator::SOCIAL_LOSS_DEN - 121_035
    );

    env.set_slot(106);
    env.send(
        pix(
            vec![
                AccountMeta::new(oracle.pubkey(), true),
                AccountMeta::new(env.market, false),
            ],
            PIx::PushAuthMark {
                asset_index: 0,
                now_slot: 106,
                mark_e6: 7,
            },
        ),
        &[&oracle],
    )
    .expect("independent oracle publishes the second adverse mark");
    env.crank(s2, 106, true);

    let market_before = env.svm.get_account(&env.market).unwrap();
    let portfolio_before = env.svm.get_account(&s2).unwrap();
    let first_liquidation = env.try_crank(s2, 106, false);
    if first_liquidation.is_err() {
        assert_eq!(env.svm.get_account(&env.market).unwrap(), market_before);
        assert_eq!(env.svm.get_account(&s2).unwrap(), portfolio_before);
    }
    if first_liquidation.is_ok() && active_leg(&portfolio_state(&env, s2)).is_none() {
        let (_, fixed_group) =
            percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
                .unwrap();
        assert_eq!(fixed_group.mode, percolator::MarketModeV16::Live);
        assert_eq!(fixed_group.assets[0].oi_eff_long_q, 0);
        assert_eq!(fixed_group.assets[0].oi_eff_short_q, 0);
        return;
    }

    let owner_reduce = env.send(
        pix(
            vec![
                AccountMeta::new(s2o.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(s2, false),
            ],
            PIx::RebalanceReduce {
                asset_index: 0,
                reduce_q: 1_162_175,
            },
        ),
        &[&s2o],
    );
    let target_forfeit = env.send(
        pix(
            vec![
                AccountMeta::new(s2o.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(s2, false),
            ],
            PIx::ForfeitRecoveryLeg {
                asset_index: 0,
                b_delta_budget: percolator::MAX_VAULT_TVL,
            },
        ),
        &[&s2o],
    );

    let fresh_owner = Keypair::new();
    let fresh = env.create_portfolio(&fresh_owner, 2_000);
    let owner_trade = env.send(
        pix(
            vec![
                AccountMeta::new(s2o.pubkey(), true),
                AccountMeta::new(fresh_owner.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(s2, false),
                AccountMeta::new(fresh, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: 1_162_175,
                exec_price: 7,
                fee_bps: 0,
            },
        ),
        &[&s2o, &fresh_owner],
    );

    let mut successful_other_cranks = 0u64;
    let mut successful_target_cranks = u64::from(first_liquidation.is_ok());
    for slot in 107..=122 {
        env.set_slot(slot);
        for portfolio in [l2, l3, l4, l5, s3] {
            if env.try_crank(portfolio, slot, false).is_ok() {
                successful_other_cranks += 1;
            }
        }
        if env.try_crank(s2, slot, false).is_ok() {
            successful_target_cranks += 1;
        }
        if active_leg(&portfolio_state(&env, s2)).is_none() {
            break;
        }
    }

    let mut survivor_results = Vec::new();
    for (label, owner, portfolio) in [
        ("l2", &l2o, l2),
        ("l3", &l3o, l3),
        ("l4", &l4o, l4),
        ("l5", &l5o, l5),
        ("s3", &s3o, s3),
    ] {
        let Some(reduce_q) =
            active_leg(&portfolio_state(&env, portfolio)).map(|leg| leg.basis_pos_q.unsigned_abs())
        else {
            survivor_results.push(format!("{label}=already-flat"));
            continue;
        };
        let reduction = env.send(
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
        );
        survivor_results.push(format!("{label}.reduce={reduction:?}"));
        if active_leg(&portfolio_state(&env, portfolio)).is_some() {
            let forfeit = env.send(
                pix(
                    vec![
                        AccountMeta::new(owner.pubkey(), true),
                        AccountMeta::new(env.market, false),
                        AccountMeta::new(portfolio, false),
                    ],
                    PIx::ForfeitRecoveryLeg {
                        asset_index: 0,
                        b_delta_budget: percolator::MAX_VAULT_TVL,
                    },
                ),
                &[owner],
            );
            survivor_results.push(format!("{label}.forfeit={forfeit:?}"));
        }
        if env.try_crank(s2, 122, false).is_ok() {
            successful_target_cranks += 1;
        }
    }

    let final_s2 = portfolio_state(&env, s2);
    let final_leg = active_leg(&final_s2);
    let (_, final_group) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    let (trapped_survivor_count, trapped_survivor_capital) = [l2, l3, l4, l5, s3]
        .into_iter()
        .filter_map(|portfolio| {
            let state = portfolio_state(&env, portfolio);
            active_leg(&state).map(|_| state.capital.get())
        })
        .fold((0u64, 0u128), |(count, capital), next| {
            (count + 1, capital + next)
        });
    assert!(
        final_leg.is_none(),
        "reset carry trapped the toxic short and independent survivors: first_liquidation={first_liquidation:?}, owner_reduce={owner_reduce:?}, target_forfeit={target_forfeit:?}, owner_trade={owner_trade:?}, survivor_results={survivor_results:?}, successful_other_cranks={successful_other_cranks}, successful_target_cranks={successful_target_cranks}, target_capital={}, target_pnl={}, target_basis={:?}, trapped_survivor_count={trapped_survivor_count}, trapped_survivor_capital={trapped_survivor_capital}, long_oi={}, short_oi={}, remainder={}, dust={}",
        final_s2.capital.get(),
        final_s2.pnl.get(),
        final_leg.map(|leg| leg.basis_pos_q),
        final_group.assets[0].oi_eff_long_q,
        final_group.assets[0].oi_eff_short_q,
        final_group.assets[0].social_loss_remainder_long_num,
        final_group.assets[0].social_loss_dust_long_num,
    );
}
