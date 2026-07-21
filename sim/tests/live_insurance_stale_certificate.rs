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

fn token_amount(svm: &LiteSVM, key: Pubkey) -> u64 {
    let account = svm.get_account(&key).unwrap();
    u64::from_le_bytes(account.data[64..72].try_into().unwrap())
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
    vault_authority: Pubkey,
    portfolio_len: usize,
}

impl Env {
    fn new() -> Self {
        const PRICE: u64 = 1_000_000;

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
            vault_authority,
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
                percolator_prog::processor::ASSET_AUTH_INSURANCE,
                provider.pubkey(),
            ),
            (
                percolator_prog::processor::ASSET_AUTH_INSURANCE_OPERATOR,
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
            .expect("separate the insurance and oracle roles");
        }

        let market_data = env.svm.get_account(&market).unwrap().data;
        let (config, _) = percolator_prog::state::read_market(&market_data).unwrap();
        let profile = percolator_prog::state::read_asset_oracle_profile(&market_data, 0).unwrap();
        assert_eq!(config.marketauth, admin.pubkey().to_bytes());
        assert_eq!(profile.asset_admin, admin.pubkey().to_bytes());
        assert_eq!(profile.insurance_authority, provider.pubkey().to_bytes(),);
        assert_eq!(profile.insurance_operator, provider.pubkey().to_bytes(),);
        assert_eq!(profile.backing_bucket_authority, admin.pubkey().to_bytes(),);
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

    fn crank(&mut self, portfolio: Pubkey, slot: u64, observe: bool) {
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
        .expect("permissionless crank makes progress");
    }

    fn close_all_resolved(&mut self, claims: &[(&str, &Keypair, Pubkey)]) -> Vec<u64> {
        let destinations = claims
            .iter()
            .map(|(_, owner, _)| {
                let destination = Pubkey::new_unique();
                set_token(
                    &mut self.svm,
                    destination,
                    self.collateral,
                    owner.pubkey(),
                    0,
                );
                destination
            })
            .collect::<Vec<_>>();
        let mut closed = vec![false; claims.len()];
        let mut last_progress = vec![None; claims.len()];
        let mut last_close = vec![None; claims.len()];
        for _ in 0..2_048 {
            for (index, (_, owner, portfolio)) in claims.iter().enumerate() {
                if closed[index] {
                    continue;
                }
                last_progress[index] = Some(self.send(
                    pix(
                        vec![
                            AccountMeta::new_readonly(owner.pubkey(), false),
                            AccountMeta::new(self.market, false),
                            AccountMeta::new(*portfolio, false),
                            AccountMeta::new(destinations[index], false),
                            AccountMeta::new(self.vault, false),
                            AccountMeta::new_readonly(self.vault_authority, false),
                            AccountMeta::new_readonly(spl_token::ID, false),
                        ],
                        PIx::CloseResolved {
                            fee_rate_per_slot: 0,
                        },
                    ),
                    &[],
                ));
                last_close[index] = Some(self.send(
                    pix(
                        vec![
                            AccountMeta::new(owner.pubkey(), true),
                            AccountMeta::new(self.market, false),
                            AccountMeta::new(*portfolio, false),
                        ],
                        PIx::ClosePortfolio,
                    ),
                    &[owner],
                ));
                closed[index] = last_close[index].as_ref().is_some_and(Result::is_ok);
            }
            if closed.iter().all(|value| *value) {
                return destinations
                    .iter()
                    .map(|destination| token_amount(&self.svm, *destination))
                    .collect();
            }
        }
        let mut diagnostics = Vec::new();
        for (index, (label, _, portfolio)) in claims.iter().enumerate() {
            if closed[index] {
                continue;
            }
            let state = percolator_prog::state::read_portfolio(
                &self.svm.get_account(portfolio).unwrap().data,
            )
            .unwrap();
            diagnostics.push(format!(
                "{label}: capital={} pnl={} bitmap={:?} close={:?} progress={:?} deregister={:?}",
                state.capital.get(),
                state.pnl.get(),
                state.active_bitmap.map(percolator::V16PodU64::get),
                state.close_progress.try_to_runtime().unwrap(),
                last_progress[index],
                last_close[index],
            ));
        }
        panic!("resolved portfolios did not close: {diagnostics:?}");
    }
}

// A live insurance operator may not release reserve capital while exposed portfolios can still
// realize loss against an already-published oracle epoch. Market-level freshness alone cannot
// prove that every losing certificate has consumed the reserve it protects.
#[test]
fn live_insurance_cannot_exit_ahead_of_stale_losing_certificate() {
    const ENTRY_PRICE: u64 = 1_000_000;
    const EXIT_PRICE: u64 = 3_000_000;
    const INSURANCE: u64 = 1_000_000;
    const LONG_CAPITAL: u64 = 3_000_000;
    const SHORT_CAPITAL: u64 = 1_100_000;

    let mut env = Env::new();
    let provider = env.provider.insecure_clone();
    let provider_source = Pubkey::new_unique();
    let provider_destination = Pubkey::new_unique();
    set_token(
        &mut env.svm,
        provider_source,
        env.collateral,
        provider.pubkey(),
        INSURANCE,
    );
    set_token(
        &mut env.svm,
        provider_destination,
        env.collateral,
        provider.pubkey(),
        0,
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
            PIx::TopUpInsuranceDomain {
                domain: 0,
                amount: u128::from(INSURANCE),
            },
        ),
        &[&provider],
    )
    .expect("provider funds the long-claim insurance domain");

    let long_owner = Keypair::new();
    let short_owner = Keypair::new();
    let observer_owner = Keypair::new();
    let long = env.create_portfolio(&long_owner, LONG_CAPITAL);
    let short = env.create_portfolio(&short_owner, SHORT_CAPITAL);
    let observer = env.create_portfolio(&observer_owner, 0);
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
                size_q: percolator::POS_SCALE as i128,
                exec_price: ENTRY_PRICE,
                fee_bps: 0,
            },
        ),
        &[&long_owner, &short_owner],
    )
    .expect("independent users open balanced exposure");

    let oracle = env.oracle.insecure_clone();
    for (slot, mark) in [(101, 2_000_000), (102, EXIT_PRICE)] {
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
        .expect("honest oracle publishes the adverse mark");
        env.crank(observer, slot, true);
    }

    let (_, published) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    let stale_short =
        percolator_prog::state::read_portfolio(&env.svm.get_account(&short).unwrap().data).unwrap();
    assert_eq!(published.assets[0].effective_price, EXIT_PRICE);
    assert_eq!(published.assets[0].raw_oracle_target_price, EXIT_PRICE);
    assert_eq!(published.assets[0].oi_eff_short_q, percolator::POS_SCALE);
    assert!(
        stale_short.health_cert.cert_oracle_epoch.get() < published.oracle_epoch,
        "the losing certificate must remain behind the fully published market epoch",
    );
    assert!(!published.bankruptcy_hlock_active);
    assert!(!published.threshold_stress_active);
    assert!(!published.loss_stale_active);

    let withdrawal = env.send(
        pix(
            vec![
                AccountMeta::new(provider.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(provider_destination, false),
                AccountMeta::new(env.vault, false),
                AccountMeta::new_readonly(env.vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::WithdrawInsuranceAsset {
                asset_index: 0,
                amount: u128::from(INSURANCE),
            },
        ),
        &[&provider],
    );
    let escaped = withdrawal.is_ok();

    env.crank(long, 102, false);
    env.crank(short, 102, false);

    let (_, settled) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    if escaped {
        assert_eq!(settled.insurance, 0);
    } else {
        assert_eq!(settled.insurance, INSURANCE as u128);
        assert_eq!(settled.assets[0].b_long_num, 0);
        assert_eq!(settled.assets[0].explicit_unallocated_loss_long, 0);
    }

    let admin = env.admin.insecure_clone();
    env.send(
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(env.market, false),
            ],
            PIx::ResolveMarket,
        ),
        &[&admin],
    )
    .expect("independent lifecycle authority resolves the accrued market");
    let payouts = env.close_all_resolved(&[
        ("long", &long_owner, long),
        ("short", &short_owner, short),
        ("observer", &observer_owner, observer),
    ]);
    let long_payout = payouts[0];
    let short_payout = payouts[1];
    assert_eq!(payouts[2], 0);

    let full_winner_payout = LONG_CAPITAL + (EXIT_PRICE - ENTRY_PRICE);
    let winner_shortfall = full_winner_payout.saturating_sub(long_payout);
    if escaped {
        assert_eq!(token_amount(&env.svm, provider_destination), INSURANCE);
        assert!(winner_shortfall > 0);
    } else {
        assert_eq!(token_amount(&env.svm, provider_destination), 0);
        assert_eq!(winner_shortfall, 0);
    }
    assert!(
        withdrawal.is_err(),
        "provider escaped {INSURANCE} insurance atoms after the market published an adverse mark but before the stale loser consumed the reserve; independent winner payout={long_payout}, expected={full_winner_payout}, shortfall={winner_shortfall}, loser payout={short_payout}",
    );
}
