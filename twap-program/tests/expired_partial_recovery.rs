use litesvm::LiteSVM;
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
    std::env::var("PERCOLATOR_PROG_SO").unwrap_or_else(|_| {
        format!(
            "{}/../target/deploy/percolator_prog.so",
            env!("CARGO_MANIFEST_DIR")
        )
    })
}

fn pix(accounts: Vec<AccountMeta>, ix: percolator_prog::ix::Instruction) -> Instruction {
    Instruction {
        program_id: perc_id(),
        accounts,
        data: ix.encode(),
    }
}

fn send(svm: &mut LiteSVM, signers: &[&Keypair], ix: Instruction) -> Result<(), String> {
    svm.expire_blockhash();
    let blockhash = svm.latest_blockhash();
    svm.send_transaction(Transaction::new_signed_with_payer(
        &[ix],
        Some(&signers[0].pubkey()),
        signers,
        blockhash,
    ))
    .map(|_| ())
    .map_err(|error| format!("{:?}", error.err))
}

fn warp_to(svm: &mut LiteSVM, slot: u64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.slot = slot;
    clock.unix_timestamp = slot as i64;
    svm.set_sysvar(&clock);
}

fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; 165];
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
            data: token_account_data(&mint, &owner, amount),
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
    let rent = svm.minimum_balance_for_rent_exemption(82);
    let instructions = [
        solana_sdk::system_instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            rent,
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
    svm.expire_blockhash();
    svm.send_transaction(Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &[payer, &mint],
        svm.latest_blockhash(),
    ))
    .unwrap();
    mint.pubkey()
}

fn vault_authority(market: Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref()], &perc_id()).0
}

fn canonical_vault(authority: Pubkey, mint: Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[authority.as_ref(), spl_token::ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

fn market_mode(svm: &LiteSVM, market: Pubkey) -> percolator::MarketModeV16 {
    percolator_prog::state::read_market_config_mode_and_capacity(
        &svm.get_account(&market).unwrap().data,
    )
    .unwrap()
    .1
}

fn close_progress(svm: &LiteSVM, portfolio: Pubkey) -> percolator::CloseProgressLedgerV16 {
    percolator_prog::state::read_portfolio(&svm.get_account(&portfolio).unwrap().data)
        .unwrap()
        .close_progress
        .try_to_runtime()
        .unwrap()
}

#[test]
#[ignore = "upstream percolator-prog#368: expired partial Recovery globally locks the market"]
fn public_expired_partial_recovery_cannot_lock_an_unrelated_depositor() {
    use percolator_prog::ix::Instruction as PIx;

    const PRICE: u64 = 1_000_000;
    const ASSET: u16 = 0;

    let mut svm =
        LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        });
    let program_path = perc_so();
    assert!(
        std::path::Path::new(&program_path).exists(),
        "missing Percolator SBF at {program_path}"
    );
    svm.add_program_from_file(perc_id(), program_path).unwrap();

    let payer = Keypair::new();
    let admin = Keypair::new();
    let oracle = Keypair::new();
    let attacker = Keypair::new();
    let counterparty = Keypair::new();
    let victim = Keypair::new();
    for signer in [&payer, &admin, &oracle, &attacker, &counterparty, &victim] {
        svm.airdrop(&signer.pubkey(), 1_000_000_000_000).unwrap();
    }
    svm.set_sysvar(&Clock::default());

    let mint_authority = Keypair::new();
    let collateral = create_mint(&mut svm, &payer, mint_authority.pubkey());
    let market_keypair = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(1).unwrap();
    let market_rent = svm.minimum_balance_for_rent_exemption(market_len);
    send(
        &mut svm,
        &[&payer, &market_keypair],
        solana_sdk::system_instruction::create_account(
            &payer.pubkey(),
            &market_keypair.pubkey(),
            market_rent,
            market_len as u64,
            &perc_id(),
        ),
    )
    .expect("publicly allocate the market");
    let market = market_keypair.pubkey();
    send(
        &mut svm,
        &[&payer, &admin],
        pix(
            vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(collateral, false),
            ],
            PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 6_480_000,
                initial_price: PRICE,
                min_nonzero_mm_req: 599,
                min_nonzero_im_req: 600,
                maintenance_margin_bps: 500,
                initial_margin_bps: 500,
                max_trading_fee_bps: 10_000,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 5,
                liquidation_fee_cap: percolator::MAX_PROTOCOL_FEE_ABS,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 24,
                max_accrual_dt_slots: 20,
                max_abs_funding_e9_per_slot: 1_000,
                min_funding_lifetime_slots: 10_000_000,
                max_account_b_settlement_chunks: 1,
                max_bankrupt_close_chunks: 1,
                max_bankrupt_close_lifetime_slots: 1,
                public_b_chunk_atoms: 8_000,
                maintenance_fee_per_slot: 0,
            },
        ),
    )
    .expect("publicly initialize a production-shaped market");
    send(
        &mut svm,
        &[&payer, &admin],
        pix(
            vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::ConfigurePermissionlessResolve {
                stale_slots: 100,
                force_close_delay_slots: 5,
            },
        ),
    )
    .expect("configure bounded public recovery");
    send(
        &mut svm,
        &[&payer, &admin, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::UpdateAssetAuthority {
                asset_index: ASSET,
                kind: 4,
                new_pubkey: oracle.pubkey().to_bytes(),
            },
        ),
    )
    .expect("bind an independent authenticated-mark authority");
    send(
        &mut svm,
        &[&payer, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::ConfigureAuthMark {
                asset_index: ASSET,
                now_slot: 0,
                initial_mark_e6: PRICE,
            },
        ),
    )
    .expect("configure the authenticated mark");

    let vault_authority = vault_authority(market);
    let vault = canonical_vault(vault_authority, collateral);
    set_token(&mut svm, vault, collateral, vault_authority, 0);

    let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    let portfolio_rent = svm.minimum_balance_for_rent_exemption(portfolio_len);
    let mut create_portfolio = |owner: &Keypair, capital: u128| {
        let portfolio = Keypair::new();
        send(
            &mut svm,
            &[&payer, &portfolio],
            solana_sdk::system_instruction::create_account(
                &payer.pubkey(),
                &portfolio.pubkey(),
                portfolio_rent,
                portfolio_len as u64,
                &perc_id(),
            ),
        )
        .expect("publicly allocate a portfolio");
        send(
            &mut svm,
            &[&payer, owner],
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio.pubkey(), false),
                ],
                PIx::InitPortfolio,
            ),
        )
        .expect("publicly initialize a portfolio");
        let token_account = Pubkey::new_unique();
        set_token(
            &mut svm,
            token_account,
            collateral,
            owner.pubkey(),
            capital as u64,
        );
        send(
            &mut svm,
            &[&payer, owner],
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio.pubkey(), false),
                    AccountMeta::new(token_account, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Deposit { amount: capital },
            ),
        )
        .expect("deposit real SPL collateral");
        (portfolio.pubkey(), token_account)
    };
    let (attacker_portfolio, _) = create_portfolio(&attacker, 51_000);
    let (counterparty_portfolio, _) = create_portfolio(&counterparty, 100_000);
    let (victim_portfolio, victim_destination) = create_portfolio(&victim, 100);

    send(
        &mut svm,
        &[&payer, &attacker, &counterparty],
        pix(
            vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new_readonly(counterparty.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(attacker_portfolio, false),
                AccountMeta::new(counterparty_portfolio, false),
            ],
            PIx::TradeNoCpi {
                asset_index: ASSET,
                size_q: percolator::POS_SCALE as i128,
                exec_price: PRICE,
                fee_bps: 0,
            },
        ),
    )
    .expect("open the public balanced pair");

    for (slot, mark) in [(20, 952_000), (25, 940_000), (30, 940_576)] {
        warp_to(&mut svm, slot);
        send(
            &mut svm,
            &[&payer, &oracle],
            pix(
                vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::PushAuthMark {
                    asset_index: ASSET,
                    now_slot: slot,
                    mark_e6: mark,
                },
            ),
        )
        .expect("publish the bounded adverse mark");
        send(
            &mut svm,
            &[&payer],
            pix(
                vec![
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(counterparty_portfolio, false),
                ],
                PIx::PermissionlessCrank {
                    now_slot: slot,
                    observations: vec![percolator_prog::ix::CrankObservationHint {
                        asset_index: ASSET,
                        oracle_accounts: 0,
                    }],
                },
            ),
        )
        .expect("materialize the bounded mark");
    }
    send(
        &mut svm,
        &[&payer, &admin],
        pix(
            vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::UpdateAssetLifecycle {
                action: 3,
                asset_index: ASSET,
                now_slot: 30,
                initial_price: 0,
                insurance_authority: [0u8; 32],
                insurance_operator: [0u8; 32],
                backing_bucket_authority: [0u8; 32],
                oracle_authority: [0u8; 32],
            },
        ),
    )
    .expect("normal administration shuts down the failed asset");
    send(
        &mut svm,
        &[&payer, &attacker],
        pix(
            vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(attacker_portfolio, false),
            ],
            PIx::ForfeitRecoveryLeg {
                asset_index: ASSET,
                b_delta_budget: percolator::MAX_VAULT_TVL,
            },
        ),
    )
    .expect("the owner commits one bounded recovery chunk");

    let partial = close_progress(&svm, attacker_portfolio);
    assert!(
        partial.active && partial.residual_remaining > 0,
        "the owner must leave a genuine bounded residual: {partial:?}"
    );
    assert_eq!(market_mode(&svm, market), percolator::MarketModeV16::Live);

    warp_to(&mut svm, partial.max_close_slot + 1);
    send(
        &mut svm,
        &[&payer],
        pix(
            vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(attacker_portfolio, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: 0,
                observations: vec![],
            },
        ),
    )
    .expect("the expired account-local ledger selects terminal recovery");

    let terminal_mode = market_mode(&svm, market);
    if terminal_mode == percolator::MarketModeV16::Recovery {
        let locked_market = svm.get_account(&market).unwrap();
        let locked_attacker = svm.get_account(&attacker_portfolio).unwrap();
        let locked_counterparty = svm.get_account(&counterparty_portfolio).unwrap();
        let locked_victim = svm.get_account(&victim_portfolio).unwrap();
        let locked_vault = svm.get_account(&vault).unwrap();
        let assert_rollback = |svm: &LiteSVM| {
            assert_eq!(svm.get_account(&market).unwrap(), locked_market);
            assert_eq!(
                svm.get_account(&attacker_portfolio).unwrap(),
                locked_attacker
            );
            assert_eq!(
                svm.get_account(&counterparty_portfolio).unwrap(),
                locked_counterparty
            );
            assert_eq!(svm.get_account(&victim_portfolio).unwrap(), locked_victim);
            assert_eq!(svm.get_account(&vault).unwrap(), locked_vault);
        };

        for (label, portfolio) in [
            ("attacker", attacker_portfolio),
            ("counterparty", counterparty_portfolio),
            ("victim", victim_portfolio),
        ] {
            let retry = send(
                &mut svm,
                &[&payer],
                pix(
                    vec![
                        AccountMeta::new_readonly(payer.pubkey(), true),
                        AccountMeta::new(market, false),
                        AccountMeta::new(portfolio, false),
                    ],
                    PIx::PermissionlessCrank {
                        now_slot: 0,
                        observations: vec![],
                    },
                ),
            );
            if retry.is_ok() {
                assert_eq!(
                    market_mode(&svm, market),
                    percolator::MarketModeV16::Recovery,
                    "{label} keeper retry unexpectedly escaped Recovery"
                );
            }
            assert_rollback(&svm);
        }

        let resolve = send(
            &mut svm,
            &[&payer, &admin],
            pix(
                vec![
                    AccountMeta::new_readonly(admin.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::ResolveMarket,
            ),
        );
        assert!(
            resolve.is_err(),
            "the configured market authority unexpectedly escaped Recovery"
        );
        assert_rollback(&svm);

        for (owner, portfolio) in [
            (&attacker, attacker_portfolio),
            (&counterparty, counterparty_portfolio),
        ] {
            let forfeit = send(
                &mut svm,
                &[&payer, owner],
                pix(
                    vec![
                        AccountMeta::new_readonly(owner.pubkey(), true),
                        AccountMeta::new(market, false),
                        AccountMeta::new(portfolio, false),
                    ],
                    PIx::ForfeitRecoveryLeg {
                        asset_index: ASSET,
                        b_delta_budget: u128::MAX,
                    },
                ),
            );
            assert!(
                forfeit.is_err(),
                "an owner forfeit unexpectedly escaped global Recovery"
            );
            assert_rollback(&svm);
        }

        let cure = send(
            &mut svm,
            &[&payer, &attacker],
            pix(
                vec![
                    AccountMeta::new_readonly(attacker.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(attacker_portfolio, false),
                ],
                PIx::CureAndCancelClose {
                    optional_deposit: 0,
                },
            ),
        );
        assert!(
            cure.is_err(),
            "a zero-deposit cure unexpectedly escaped Recovery"
        );
        assert_rollback(&svm);

        let reduce = send(
            &mut svm,
            &[&payer, &attacker],
            pix(
                vec![
                    AccountMeta::new_readonly(attacker.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(attacker_portfolio, false),
                ],
                PIx::RebalanceReduce {
                    asset_index: ASSET,
                    reduce_q: u128::MAX,
                },
            ),
        );
        assert!(
            reduce.is_err(),
            "owner risk reduction unexpectedly escaped Recovery"
        );
        assert_rollback(&svm);

        let victim_close = send(
            &mut svm,
            &[&payer, &victim],
            pix(
                vec![
                    AccountMeta::new_readonly(victim.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(victim_portfolio, false),
                    AccountMeta::new(victim_destination, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::CloseResolved {
                    fee_rate_per_slot: 0,
                },
            ),
        );
        assert!(
            victim_close.is_err(),
            "an unrelated flat user unexpectedly withdrew from Recovery"
        );
        assert_rollback(&svm);
    }

    assert_eq!(
        terminal_mode,
        percolator::MarketModeV16::Resolved,
        "expired account-local recovery must not create a global mode from which keeper, owner, \
         and configured authority continuations all roll back"
    );

    let vault_before = token_amount(&svm, vault);
    send(
        &mut svm,
        &[&payer, &victim],
        pix(
            vec![
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(victim_portfolio, false),
                AccountMeta::new(victim_destination, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::CloseResolved {
                fee_rate_per_slot: 0,
            },
        ),
    )
    .expect("the unrelated flat owner retains the bounded custody exit");
    assert_eq!(token_amount(&svm, victim_destination), 100);
    assert_eq!(token_amount(&svm, vault), vault_before - 100);
}
