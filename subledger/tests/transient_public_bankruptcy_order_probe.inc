fn fresh_public_cross_backing_market() -> Env {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = Env::new_cross_backing();
    let market = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let market_rent = env.svm.minimum_balance_for_rent_exemption(market_len);
    env.send(
        &[system_instruction::create_account(
            &env.payer.pubkey(),
            &market.pubkey(),
            market_rent,
            market_len as u64,
            &perc_id(),
        )],
        &[&market],
    )
    .expect("publicly allocate the fresh market");

    env.slab = market.pubkey();
    env.vault_authority =
        Pubkey::find_program_address(&[b"vault", env.slab.as_ref()], &perc_id()).0;
    env.perc_vault = Pubkey::find_program_address(
        &[
            env.vault_authority.as_ref(),
            spl_token::ID.as_ref(),
            env.mint.as_ref(),
        ],
        &ATA_PROGRAM_ID,
    )
    .0;
    env.svm
        .set_account(
            env.perc_vault,
            Account {
                lamports: 1_000_000,
                data: token_account_data(&env.mint, &env.vault_authority, 0),
                owner: spl_token::ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.pool = cross_backing_pool_pda_with_schedule(
        &env.mint,
        &env.coin_mint,
        &env.slab,
        POLICY_PRINCIPAL,
        env.deposit_window_slots,
        env.bootstrap_start_slot,
        env.bootstrap_delay_slots,
    );

    let market_admin = clone_kp(&env.market_admin);
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(market_admin.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new_readonly(env.mint, false),
            ],
            data: PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 10,
                initial_price: 100,
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
            }
            .encode(),
        }],
        &[&market_admin],
    )
    .expect("publicly initialize the fresh market");
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(market_admin.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ConfigureAuthMark {
                asset_index: 0,
                now_slot: 100,
                initial_mark_e6: 100,
            }
            .encode(),
        }],
        &[&market_admin],
    )
    .expect("configure the fresh market's authenticated mark");
    env.init_cross_backing_genesis_pool();
    env
}

fn create_fully_public_portfolio(env: &mut Env, owner: &Keypair, capital: u64) -> Pubkey {
    use percolator_prog::ix::Instruction as PIx;

    env.svm.airdrop(&owner.pubkey(), 1_000_000_000).unwrap();
    let portfolio = Keypair::new();
    let portfolio_len =
        percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    let portfolio_rent = env.svm.minimum_balance_for_rent_exemption(portfolio_len);
    env.send(
        &[system_instruction::create_account(
            &env.payer.pubkey(),
            &portfolio.pubkey(),
            portfolio_rent,
            portfolio_len as u64,
            &perc_id(),
        )],
        &[&portfolio],
    )
    .expect("publicly allocate a fresh portfolio");
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(owner.pubkey(), true),
                AccountMeta::new(env.slab, false),
                AccountMeta::new(portfolio.pubkey(), false),
            ],
            data: PIx::InitPortfolio.encode(),
        }],
        &[owner],
    )
    .expect("publicly initialize the fresh portfolio");

    if capital != 0 {
        let payer = clone_kp(&env.payer);
        let mint_auth = clone_kp(&env.mint_auth);
        let source = create_token_account(&mut env.svm, &payer, &env.mint, &owner.pubkey());
        mint_to(
            &mut env.svm,
            &payer,
            &env.mint,
            &mint_auth,
            &source,
            capital,
        );
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(portfolio.pubkey(), false),
                    AccountMeta::new(source, false),
                    AccountMeta::new(env.perc_vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                data: PIx::Deposit {
                    amount: u128::from(capital),
                }
                .encode(),
            }],
            &[owner],
        )
        .expect("fund the fresh public portfolio");
    }
    portfolio.pubkey()
}

fn close_resolved_in_order(
    env: &mut Env,
    actors: &[(&Keypair, Pubkey, Pubkey)],
    order: &[usize],
) {
    use percolator_prog::ix::Instruction as PIx;

    for _ in 0..512 {
        let mut open = 0usize;
        for index in order {
            let (owner, portfolio, destination) = actors[*index];
            if env
                .svm
                .get_account(&portfolio)
                .map_or(true, |account| account.lamports == 0)
            {
                continue;
            }
            open += 1;
            let _ = env.send(
                &[Instruction {
                    program_id: perc_id(),
                    accounts: vec![
                        AccountMeta::new_readonly(owner.pubkey(), true),
                        AccountMeta::new(env.slab, false),
                        AccountMeta::new(portfolio, false),
                        AccountMeta::new(destination, false),
                        AccountMeta::new(env.perc_vault, false),
                        AccountMeta::new_readonly(env.vault_authority, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    data: PIx::CloseResolved {
                        fee_rate_per_slot: 0,
                    }
                    .encode(),
                }],
                &[owner],
            );
            let _ = env.send(
                &[Instruction {
                    program_id: perc_id(),
                    accounts: vec![
                        AccountMeta::new_readonly(owner.pubkey(), true),
                        AccountMeta::new(env.slab, false),
                        AccountMeta::new(portfolio, false),
                        AccountMeta::new(destination, false),
                        AccountMeta::new(env.perc_vault, false),
                        AccountMeta::new_readonly(env.vault_authority, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    data: PIx::ClaimResolvedPayoutTopup.encode(),
                }],
                &[owner],
            );
            let _ = env.send(
                &[Instruction {
                    program_id: perc_id(),
                    accounts: vec![
                        AccountMeta::new_readonly(owner.pubkey(), true),
                        AccountMeta::new(env.slab, false),
                        AccountMeta::new(portfolio, false),
                    ],
                    data: PIx::ClosePortfolio.encode(),
                }],
                &[owner],
            );
        }
        if open == 0 {
            return;
        }
    }
    panic!("ordered resolved cleanup did not converge");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PublicBankruptcyOrderOutcome {
    trader_payouts: [u64; 4],
    provider_payout: u64,
    vault_remainder: u64,
    escrow_remainder: u64,
}

impl PublicBankruptcyOrderOutcome {
    fn total_collateral(self) -> u64 {
        self.trader_payouts
            .into_iter()
            .sum::<u64>()
            .checked_add(self.provider_payout)
            .and_then(|value| value.checked_add(self.vault_remainder))
            .and_then(|value| value.checked_add(self.escrow_remainder))
            .unwrap()
    }
}

fn run_public_bankruptcy_order(
    order: [usize; 5],
    live_bankruptcy_order: Option<[usize; 2]>,
) -> PublicBankruptcyOrderOutcome {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = fresh_public_cross_backing_market();
    let provider_principal = 201u64;
    let (provider, provider_ata) = new_depositor(&mut env, provider_principal);
    let pool_holding = create_canonical_pool_holding(&mut env);
    env.cross_backing_deposit(
        &provider,
        &provider_ata,
        &pool_holding,
        provider_principal,
    )
    .expect("fund the real cross-backed genesis waterfall");

    let long_a = Keypair::new();
    let short_a = Keypair::new();
    let long_b = Keypair::new();
    let short_b = Keypair::new();
    let observer = Keypair::new();
    let long_a_portfolio = create_fully_public_portfolio(&mut env, &long_a, 1_000_000);
    let short_a_portfolio = create_fully_public_portfolio(&mut env, &short_a, 120);
    let long_b_portfolio = create_fully_public_portfolio(&mut env, &long_b, 1_000_000);
    let short_b_portfolio = create_fully_public_portfolio(&mut env, &short_b, 180);
    let observer_portfolio = create_fully_public_portfolio(&mut env, &observer, 0);

    for (long, long_portfolio, short, short_portfolio) in [
        (&long_a, long_a_portfolio, &short_a, short_a_portfolio),
        (&long_b, long_b_portfolio, &short_b, short_b_portfolio),
    ] {
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(long.pubkey(), true),
                    AccountMeta::new_readonly(short.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(long_portfolio, false),
                    AccountMeta::new(short_portfolio, false),
                ],
                data: PIx::TradeNoCpi {
                    asset_index: 0,
                    size_q: percolator::POS_SCALE as i128,
                    exec_price: 100,
                    fee_bps: 0,
                }
                .encode(),
            }],
            &[long, short],
        )
        .expect("open an independent public long/short pair");
    }

    let market_admin = clone_kp(&env.market_admin);
    let mut slot = env.svm.get_sysvar::<Clock>().slot;
    advance_public_mark(
        &mut env,
        &market_admin,
        observer_portfolio,
        &mut slot,
        301,
        8,
    );
    if let Some(live_order) = live_bankruptcy_order {
        let bankrupt_portfolios = [short_a_portfolio, short_b_portfolio];
        for index in live_order {
            liquidate_stale_public_loser(&mut env, bankrupt_portfolios[index], slot);
        }
    }
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(market_admin.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&market_admin],
    )
    .expect("resolve the fresh market at the reached mark");

    let payer = clone_kp(&env.payer);
    let actors_and_portfolios = [
        (&long_a, long_a_portfolio),
        (&short_a, short_a_portfolio),
        (&long_b, long_b_portfolio),
        (&short_b, short_b_portfolio),
        (&observer, observer_portfolio),
    ];
    let destinations = actors_and_portfolios.map(|(owner, _)| {
        create_token_account(&mut env.svm, &payer, &env.mint, &owner.pubkey())
    });
    let actors = [
        (&long_a, long_a_portfolio, destinations[0]),
        (&short_a, short_a_portfolio, destinations[1]),
        (&long_b, long_b_portfolio, destinations[2]),
        (&short_b, short_b_portfolio, destinations[3]),
        (&observer, observer_portfolio, destinations[4]),
    ];
    close_resolved_in_order(&mut env, &actors, &order);

    env.cross_backing_withdraw(
        &provider,
        &provider_ata,
        &pool_holding,
        provider_principal,
    )
    .expect("the independent provider realizes the terminal waterfall loss");

    PublicBankruptcyOrderOutcome {
        trader_payouts: [
            env.token_amount(&destinations[0]),
            env.token_amount(&destinations[1]),
            env.token_amount(&destinations[2]),
            env.token_amount(&destinations[3]),
        ],
        provider_payout: env.token_amount(&provider_ata),
        vault_remainder: env.token_amount(&env.perc_vault),
        escrow_remainder: env.token_amount(&pool_holding),
    }
}

#[test]
fn public_bankruptcy_order_cannot_shift_loss_into_cross_backing_provider() {
    let first_loser_first = run_public_bankruptcy_order([1, 3, 0, 2, 4], None);
    let second_loser_first = run_public_bankruptcy_order([3, 1, 2, 0, 4], None);

    assert_eq!(
        first_loser_first, second_loser_first,
        "a public closer cannot choose which trader or provider absorbs the scarce waterfall",
    );
    assert!(
        first_loser_first.provider_payout < 201,
        "the provider must absorb a real non-vacuous market loss",
    );
    assert_eq!(
        first_loser_first.total_collateral(),
        2_000_501,
        "all deposited collateral remains accounted for after terminal cleanup",
    );
}

#[test]
fn public_live_bankruptcy_crank_order_cannot_redistribute_provider_loss() {
    let first_loser_first =
        run_public_bankruptcy_order([1, 3, 0, 2, 4], Some([0, 1]));
    let second_loser_first =
        run_public_bankruptcy_order([1, 3, 0, 2, 4], Some([1, 0]));

    assert_eq!(
        first_loser_first, second_loser_first,
        "public liquidation order redistributed trader payouts or provider loss",
    );
    assert!(
        first_loser_first.provider_payout < 201,
        "the public liquidations must consume real provider protection",
    );
    assert_eq!(
        first_loser_first.total_collateral(),
        2_000_501,
        "live bankruptcy processing must conserve all deposited collateral",
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PublicCodepositorExitOutcome {
    trader_payouts: [u64; 4],
    provider_payouts: [u64; 2],
    vault_remainder: u64,
    escrow_remainder: u64,
}

impl PublicCodepositorExitOutcome {
    fn total_collateral(self) -> u64 {
        self.trader_payouts
            .into_iter()
            .chain(self.provider_payouts)
            .sum::<u64>()
            .checked_add(self.vault_remainder)
            .and_then(|value| value.checked_add(self.escrow_remainder))
            .unwrap()
    }
}

fn run_public_codepositor_exit_order(
    exit_order: [usize; 2],
    first_provider_chunks: &[u64],
) -> PublicCodepositorExitOutcome {
    use percolator_prog::ix::Instruction as PIx;

    let mut env = fresh_public_cross_backing_market();
    let provider_principals = [199u64, 2u64];
    let (first_provider, first_provider_ata) =
        new_depositor(&mut env, provider_principals[0]);
    let (second_provider, second_provider_ata) =
        new_depositor(&mut env, provider_principals[1]);
    let pool_holding = create_canonical_pool_holding(&mut env);
    for (provider, destination, principal) in [
        (
            &first_provider,
            &first_provider_ata,
            provider_principals[0],
        ),
        (
            &second_provider,
            &second_provider_ata,
            provider_principals[1],
        ),
    ] {
        env.cross_backing_deposit(provider, destination, &pool_holding, principal)
            .expect("fund one independently attributed backing position");
    }

    let long_a = Keypair::new();
    let short_a = Keypair::new();
    let long_b = Keypair::new();
    let short_b = Keypair::new();
    let observer = Keypair::new();
    let long_a_portfolio = create_fully_public_portfolio(&mut env, &long_a, 1_000_000);
    let short_a_portfolio = create_fully_public_portfolio(&mut env, &short_a, 120);
    let long_b_portfolio = create_fully_public_portfolio(&mut env, &long_b, 1_000_000);
    let short_b_portfolio = create_fully_public_portfolio(&mut env, &short_b, 180);
    let observer_portfolio = create_fully_public_portfolio(&mut env, &observer, 0);
    for (long, long_portfolio, short, short_portfolio) in [
        (&long_a, long_a_portfolio, &short_a, short_a_portfolio),
        (&long_b, long_b_portfolio, &short_b, short_b_portfolio),
    ] {
        env.send(
            &[Instruction {
                program_id: perc_id(),
                accounts: vec![
                    AccountMeta::new_readonly(long.pubkey(), true),
                    AccountMeta::new_readonly(short.pubkey(), true),
                    AccountMeta::new(env.slab, false),
                    AccountMeta::new(long_portfolio, false),
                    AccountMeta::new(short_portfolio, false),
                ],
                data: PIx::TradeNoCpi {
                    asset_index: 0,
                    size_q: percolator::POS_SCALE as i128,
                    exec_price: 100,
                    fee_bps: 0,
                }
                .encode(),
            }],
            &[long, short],
        )
        .expect("open an independent public long/short pair");
    }

    let market_admin = clone_kp(&env.market_admin);
    let mut slot = env.svm.get_sysvar::<Clock>().slot;
    advance_public_mark(
        &mut env,
        &market_admin,
        observer_portfolio,
        &mut slot,
        301,
        8,
    );
    env.send(
        &[Instruction {
            program_id: perc_id(),
            accounts: vec![
                AccountMeta::new_readonly(market_admin.pubkey(), true),
                AccountMeta::new(env.slab, false),
            ],
            data: PIx::ResolveMarket.encode(),
        }],
        &[&market_admin],
    )
    .expect("resolve the fresh market at the reached mark");

    let payer = clone_kp(&env.payer);
    let actors_and_portfolios = [
        (&long_a, long_a_portfolio),
        (&short_a, short_a_portfolio),
        (&long_b, long_b_portfolio),
        (&short_b, short_b_portfolio),
        (&observer, observer_portfolio),
    ];
    let destinations = actors_and_portfolios.map(|(owner, _)| {
        create_token_account(&mut env.svm, &payer, &env.mint, &owner.pubkey())
    });
    let actors = [
        (&long_a, long_a_portfolio, destinations[0]),
        (&short_a, short_a_portfolio, destinations[1]),
        (&long_b, long_b_portfolio, destinations[2]),
        (&short_b, short_b_portfolio, destinations[3]),
        (&observer, observer_portfolio, destinations[4]),
    ];
    close_resolved_in_order(&mut env, &actors, &[1, 3, 0, 2, 4]);

    assert_eq!(
        first_provider_chunks.iter().sum::<u64>(),
        provider_principals[0],
    );
    for provider_index in exit_order {
        if provider_index == 0 {
            for amount in first_provider_chunks {
                env.cross_backing_withdraw(
                    &first_provider,
                    &first_provider_ata,
                    &pool_holding,
                    *amount,
                )
                .expect("first provider makes bounded withdrawal progress");
            }
        } else {
            env.cross_backing_withdraw(
                &second_provider,
                &second_provider_ata,
                &pool_holding,
                provider_principals[1],
            )
            .expect("second provider makes bounded withdrawal progress");
        }
    }

    PublicCodepositorExitOutcome {
        trader_payouts: [
            env.token_amount(&destinations[0]),
            env.token_amount(&destinations[1]),
            env.token_amount(&destinations[2]),
            env.token_amount(&destinations[3]),
        ],
        provider_payouts: [
            env.token_amount(&first_provider_ata),
            env.token_amount(&second_provider_ata),
        ],
        vault_remainder: env.token_amount(&env.perc_vault),
        escrow_remainder: env.token_amount(&pool_holding),
    }
}

#[test]
fn public_cross_backing_codepositor_exit_order_cannot_transfer_the_victims_claim() {
    let control = run_public_codepositor_exit_order([0, 1], &[199]);
    let victim_first = run_public_codepositor_exit_order([1, 0], &[199]);
    let split = run_public_codepositor_exit_order([0, 1], &[1, 198]);

    assert_eq!(
        control, victim_first,
        "full-exit ordering redistributed an independent backing claim",
    );
    assert!(
        split.provider_payouts[0] <= control.provider_payouts[0],
        "splitting extracted backing from the common waterfall",
    );
    assert!(
        split.provider_payouts[1] >= control.provider_payouts[1],
        "splitting reduced the independent provider's backing payout",
    );
    for outcome in [control, victim_first, split] {
        assert_eq!(
            outcome.total_collateral(),
            2_000_501,
            "all public deposits remain accounted for after owner exits",
        );
    }
}
