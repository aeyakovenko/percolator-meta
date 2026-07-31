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
    if let Ok(path) = std::env::var("PERCOLATOR_TEST_SO") {
        assert!(
            std::path::Path::new(&path).exists(),
            "missing requested Percolator SBF at {path}"
        );
        return path;
    }
    let pinned = format!(
        "{}/../target/deploy/percolator_prog.so",
        env!("CARGO_MANIFEST_DIR")
    );
    assert!(
        std::path::Path::new(&pinned).exists(),
        "missing pinned Percolator SBF at {pinned}"
    );
    pinned
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

fn has_active_leg(svm: &LiteSVM, portfolio: Pubkey, asset_index: usize) -> bool {
    let account = svm.get_account(&portfolio).unwrap();
    percolator_prog::state::read_portfolio(&account.data)
        .unwrap()
        .legs
        .iter()
        .filter_map(|leg| leg.try_to_runtime().ok())
        .any(|leg| leg.active && leg.asset_index as usize == asset_index)
}

#[test]
#[ignore = "upstream percolator-prog#371: winner-first Recovery can strand the bankrupt leg"]
fn public_winner_first_recovery_forfeit_cannot_strand_the_loser() {
    use percolator_prog::ix::Instruction as PIx;

    const PRICE: u64 = 1_000_000;
    const ASSET: u16 = 1;
    const WINNER_SOURCE_DOMAIN: u16 = ASSET * 2;
    const BACKING: u128 = 100_000;
    const BASE_RISK_Q: i128 = percolator::POS_SCALE as i128 * 3 / 2;

    let mut svm =
        LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();

    let payer = Keypair::new();
    let admin = Keypair::new();
    let oracle = Keypair::new();
    let provider = Keypair::new();
    let loser = Keypair::new();
    let winner = Keypair::new();
    let base_counterparty = Keypair::new();
    for signer in [
        &payer,
        &admin,
        &oracle,
        &provider,
        &loser,
        &winner,
        &base_counterparty,
    ] {
        svm.airdrop(&signer.pubkey(), 1_000_000_000_000).unwrap();
    }
    svm.set_sysvar(&Clock::default());

    let mint_authority = Keypair::new();
    let collateral = create_mint(&mut svm, &payer, mint_authority.pubkey());
    let market_keypair = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(2).unwrap();
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
                max_portfolio_assets: 2,
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
                max_bankrupt_close_lifetime_slots: 100,
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
        &[&payer, &admin, &provider],
        pix(
            vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new_readonly(provider.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::UpdateAssetAuthority {
                asset_index: ASSET,
                kind: 3,
                new_pubkey: provider.pubkey().to_bytes(),
            },
        ),
    )
    .expect("bind an independent backing provider");
    for asset_index in [0u16, ASSET] {
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
                    asset_index,
                    kind: 4,
                    new_pubkey: oracle.pubkey().to_bytes(),
                },
            ),
        )
        .expect("bind the independent oracle");
        send(
            &mut svm,
            &[&payer, &oracle],
            pix(
                vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index,
                    now_slot: 0,
                    initial_mark_e6: PRICE,
                },
            ),
        )
        .expect("configure an authenticated mark");
    }

    let vault_authority = vault_authority(market);
    let vault = canonical_vault(vault_authority, collateral);
    set_token(&mut svm, vault, collateral, vault_authority, 0);
    let provider_source = Pubkey::new_unique();
    set_token(
        &mut svm,
        provider_source,
        collateral,
        provider.pubkey(),
        BACKING as u64,
    );
    send(
        &mut svm,
        &[&payer, &provider],
        pix(
            vec![
                AccountMeta::new_readonly(provider.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(provider_source, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::TopUpBackingBucket {
                domain: WINNER_SOURCE_DOMAIN,
                amount: BACKING,
                expiry_slot: 10_000,
            },
        ),
    )
    .expect("fund the winner's backing domain");
    assert_eq!(token_amount(&svm, provider_source), 0);

    let loser_portfolio = Keypair::new();
    let winner_portfolio = Keypair::new();
    let base_counterparty_portfolio = Keypair::new();
    let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(2).unwrap();
    let portfolio_rent = svm.minimum_balance_for_rent_exemption(portfolio_len);
    for (owner, portfolio, capital) in [
        (&loser, &loser_portfolio, 51_000u128),
        (&winner, &winner_portfolio, 100_000),
        (&base_counterparty, &base_counterparty_portfolio, 100_000),
    ] {
        send(
            &mut svm,
            &[&payer, portfolio],
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
        let source = Pubkey::new_unique();
        set_token(&mut svm, source, collateral, owner.pubkey(), capital as u64);
        send(
            &mut svm,
            &[&payer, owner],
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio.pubkey(), false),
                    AccountMeta::new(source, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Deposit { amount: capital },
            ),
        )
        .expect("deposit independent trader collateral");
    }
    let loser_portfolio = loser_portfolio.pubkey();
    let winner_portfolio = winner_portfolio.pubkey();
    let base_counterparty_portfolio = base_counterparty_portfolio.pubkey();

    let trade = |svm: &mut LiteSVM,
                 asset_index: u16,
                 owner_a: &Keypair,
                 portfolio_a: Pubkey,
                 owner_b: &Keypair,
                 portfolio_b: Pubkey,
                 size_q: i128,
                 price: u64| {
        send(
            svm,
            &[&payer, owner_a, owner_b],
            pix(
                vec![
                    AccountMeta::new_readonly(owner_a.pubkey(), true),
                    AccountMeta::new_readonly(owner_b.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio_a, false),
                    AccountMeta::new(portfolio_b, false),
                ],
                PIx::TradeNoCpi {
                    asset_index,
                    size_q,
                    exec_price: price,
                    fee_bps: 0,
                },
            ),
        )
    };
    let crank = |svm: &mut LiteSVM, portfolio: Pubkey, asset_index: u16, slot: u64| {
        send(
            svm,
            &[&payer],
            pix(
                vec![
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::PermissionlessCrank {
                    now_slot: slot,
                    observations: vec![percolator_prog::ix::CrankObservationHint {
                        asset_index,
                        oracle_accounts: 0,
                    }],
                },
            ),
        )
    };
    let push_mark = |svm: &mut LiteSVM, asset_index: u16, slot: u64, mark_e6: u64| {
        send(
            svm,
            &[&payer, &oracle],
            pix(
                vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::PushAuthMark {
                    asset_index,
                    now_slot: slot,
                    mark_e6,
                },
            ),
        )
    };

    trade(
        &mut svm,
        ASSET,
        &loser,
        loser_portfolio,
        &winner,
        winner_portfolio,
        percolator::POS_SCALE as i128,
        PRICE,
    )
    .expect("open the secondary-asset pair");
    warp_to(&mut svm, 20);
    push_mark(&mut svm, ASSET, 20, 952_000).expect("publish the winner's first mark");
    crank(&mut svm, winner_portfolio, ASSET, 20).expect("materialize the winner's claim");
    trade(
        &mut svm,
        0,
        &winner,
        winner_portfolio,
        &base_counterparty,
        base_counterparty_portfolio,
        BASE_RISK_Q,
        PRICE,
    )
    .expect("open independent base-asset cross-margin risk");
    let winner_state =
        percolator_prog::state::read_portfolio(&svm.get_account(&winner_portfolio).unwrap().data)
            .unwrap();
    let winner_source = percolator_prog::state::portfolio_source_domain(
        &winner_state,
        WINNER_SOURCE_DOMAIN as usize,
    );
    assert!(
        winner_source.source_lien_counterparty_backing_num.get() > 0,
        "the winner must carry a real provider-backing lien"
    );

    for (slot, mark) in [(25, 940_000), (30, 940_576)] {
        warp_to(&mut svm, slot);
        push_mark(&mut svm, ASSET, slot, mark).expect("publish a bounded adverse mark");
        crank(&mut svm, winner_portfolio, ASSET, slot)
            .expect("advance the secondary asset toward bankruptcy");
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
    .expect("normal administration shuts down the secondary asset");
    send(
        &mut svm,
        &[&payer, &winner],
        pix(
            vec![
                AccountMeta::new_readonly(winner.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(winner_portfolio, false),
            ],
            PIx::ForfeitRecoveryLeg {
                asset_index: ASSET,
                b_delta_budget: u128::MAX,
            },
        ),
    )
    .expect("the winning Recovery owner exits first");
    assert!(!has_active_leg(&svm, winner_portfolio, ASSET as usize));

    let market_after_winner = svm.get_account(&market).unwrap();
    let loser_after_winner = svm.get_account(&loser_portfolio).unwrap();
    let winner_after_winner = svm.get_account(&winner_portfolio).unwrap();
    let base_after_winner = svm.get_account(&base_counterparty_portfolio).unwrap();
    let vault_after_winner = svm.get_account(&vault).unwrap();
    let assert_winner_first_rollback = |svm: &LiteSVM| {
        assert_eq!(svm.get_account(&market).unwrap(), market_after_winner);
        assert_eq!(
            svm.get_account(&loser_portfolio).unwrap(),
            loser_after_winner
        );
        assert_eq!(
            svm.get_account(&winner_portfolio).unwrap(),
            winner_after_winner
        );
        assert_eq!(
            svm.get_account(&base_counterparty_portfolio).unwrap(),
            base_after_winner
        );
        assert_eq!(svm.get_account(&vault).unwrap(), vault_after_winner);
    };

    let loser_forfeit = send(
        &mut svm,
        &[&payer, &loser],
        pix(
            vec![
                AccountMeta::new_readonly(loser.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
            ],
            PIx::ForfeitRecoveryLeg {
                asset_index: ASSET,
                b_delta_budget: u128::MAX,
            },
        ),
    );
    assert!(
        loser_forfeit
            .as_ref()
            .is_err_and(|error| error.contains("Custom(23)")),
        "the current-pin control must reach RecoveryRequired: {loser_forfeit:?}"
    );
    assert_winner_first_rollback(&svm);

    let observed_crank = crank(&mut svm, loser_portfolio, ASSET, 30);
    assert!(
        observed_crank.is_err(),
        "an ordinary keeper crank unexpectedly cleared the dead leg"
    );
    assert_winner_first_rollback(&svm);

    let unobserved_crank = send(
        &mut svm,
        &[&payer],
        pix(
            vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: 30,
                observations: vec![],
            },
        ),
    );
    assert!(
        unobserved_crank.is_err(),
        "an observation-free keeper crank unexpectedly cleared the dead leg"
    );
    assert_winner_first_rollback(&svm);

    let reduce = send(
        &mut svm,
        &[&payer, &loser],
        pix(
            vec![
                AccountMeta::new_readonly(loser.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
            ],
            PIx::RebalanceReduce {
                asset_index: ASSET,
                reduce_q: u128::MAX,
            },
        ),
    );
    assert!(
        reduce.is_err(),
        "the bankrupt owner unexpectedly reduced the stuck Recovery leg"
    );
    assert_winner_first_rollback(&svm);

    let convert = send(
        &mut svm,
        &[&payer, &loser],
        pix(
            vec![
                AccountMeta::new_readonly(loser.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
            ],
            PIx::ConvertReleasedPnl { amount: u128::MAX },
        ),
    );
    assert!(
        convert.is_err(),
        "PnL conversion unexpectedly cleared the bankrupt leg"
    );
    assert_winner_first_rollback(&svm);

    let cure = send(
        &mut svm,
        &[&payer, &loser],
        pix(
            vec![
                AccountMeta::new_readonly(loser.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
            ],
            PIx::CureAndCancelClose {
                optional_deposit: 0,
            },
        ),
    );
    assert!(
        cure.is_err(),
        "the zero-deposit cure unexpectedly cleared the bankrupt leg"
    );
    assert_winner_first_rollback(&svm);

    let assisted_close = trade(
        &mut svm,
        ASSET,
        &loser,
        loser_portfolio,
        &base_counterparty,
        base_counterparty_portfolio,
        -(percolator::POS_SCALE as i128),
        940_576,
    );
    assert!(
        assisted_close.is_err(),
        "a fresh counterparty unexpectedly reopened the shutdown side"
    );
    assert_winner_first_rollback(&svm);

    let close_portfolio = send(
        &mut svm,
        &[&payer, &loser],
        pix(
            vec![
                AccountMeta::new_readonly(loser.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
            ],
            PIx::ClosePortfolio,
        ),
    );
    assert!(
        close_portfolio.is_err(),
        "the owner unexpectedly closed a portfolio with a stuck active leg"
    );
    assert_winner_first_rollback(&svm);

    warp_to(&mut svm, 35);
    let force_close = send(
        &mut svm,
        &[&payer],
        pix(
            vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
                AccountMeta::new(winner_portfolio, false),
            ],
            PIx::ForceCloseAbandonedAsset {
                asset_index: ASSET,
                now_slot: 35,
                close_q: u128::MAX,
            },
        ),
    );
    assert!(
        force_close.is_err(),
        "the delayed public force-close unexpectedly found an opposing leg"
    );
    assert_winner_first_rollback(&svm);

    let backing_after_local_attempts = percolator_accounting::read_asset_backing_balances(
        &svm.get_account(&market).unwrap().data,
        ASSET as usize,
    )
    .unwrap()[0];
    assert!(
        backing_after_local_attempts.valid_liened_principal_atoms > 0,
        "winner-first exit must leave real provider principal stuck on the current pin"
    );
    assert!(has_active_leg(&svm, loser_portfolio, ASSET as usize));

    warp_to(&mut svm, 1_000);
    push_mark(&mut svm, 0, 1_000, PRICE).expect("keep the unrelated base oracle current");
    crank(&mut svm, winner_portfolio, 0, 1_000)
        .expect("honest base cranking keeps the rest of the market live");
    let mode = percolator_prog::state::read_market_config_mode_and_capacity(
        &svm.get_account(&market).unwrap().data,
    )
    .unwrap()
    .1;
    assert_eq!(mode, percolator::MarketModeV16::Live);

    let market_after_base_crank = svm.get_account(&market).unwrap();
    let loser_after_base_crank = svm.get_account(&loser_portfolio).unwrap();
    let loser_retry = send(
        &mut svm,
        &[&payer, &loser],
        pix(
            vec![
                AccountMeta::new_readonly(loser.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(loser_portfolio, false),
            ],
            PIx::ForfeitRecoveryLeg {
                asset_index: ASSET,
                b_delta_budget: u128::MAX,
            },
        ),
    );
    assert!(
        loser_retry
            .as_ref()
            .is_err_and(|error| error.contains("Custom(23)")),
        "fresh unrelated cranking unexpectedly repaired the local Recovery state: {loser_retry:?}"
    );
    assert_eq!(svm.get_account(&market).unwrap(), market_after_base_crank);
    assert_eq!(
        svm.get_account(&loser_portfolio).unwrap(),
        loser_after_base_crank
    );

    let stale_resolve = send(
        &mut svm,
        &[&payer],
        pix(
            vec![AccountMeta::new(market, false)],
            PIx::ResolveStalePermissionless { now_slot: 1_000 },
        ),
    );
    assert!(
        stale_resolve.is_err(),
        "a freshly cranked base asset must keep global stale resolution unavailable"
    );
    assert_eq!(svm.get_account(&market).unwrap(), market_after_base_crank);

    assert!(
        loser_forfeit.is_ok(),
        "winner-first Recovery permanently strands the bankrupt owner and {} provider atoms: first={loser_forfeit:?}, retry={loser_retry:?}, observed_crank={observed_crank:?}, unobserved_crank={unobserved_crank:?}, reduce={reduce:?}, convert={convert:?}, cure={cure:?}, assisted_close={assisted_close:?}, close_portfolio={close_portfolio:?}, force_close={force_close:?}",
        backing_after_local_attempts.valid_liened_principal_atoms,
    );
}

#[test]
#[ignore = "upstream percolator-prog#372: overlapping Recovery can strand settled profit"]
fn public_reopened_recovery_forfeit_capitalizes_a_prior_overlapped_claim() {
    use percolator_prog::ix::Instruction as PIx;

    const PRICE: u64 = 1_000_000;
    const PROFIT_MARK: u64 = 952_000;
    const SECOND_PROFIT_MARK: u64 = 932_000;
    const ASSET: u16 = 1;
    const SOURCE_DOMAIN: u16 = ASSET * 2;
    const BACKING: u128 = 100_000;
    const CAPITAL: u128 = 1_000_000;
    const RETAINED_CAPITAL: u128 = 5_000;

    let mut svm =
        LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();

    let payer = Keypair::new();
    let admin = Keypair::new();
    let oracle = Keypair::new();
    let attacker = Keypair::new();
    let victim = Keypair::new();
    let first_counterparty = Keypair::new();
    for signer in [
        &payer,
        &admin,
        &oracle,
        &attacker,
        &victim,
        &first_counterparty,
    ] {
        svm.airdrop(&signer.pubkey(), 1_000_000_000_000).unwrap();
    }
    svm.set_sysvar(&Clock::default());

    let mint_authority = Keypair::new();
    let collateral = create_mint(&mut svm, &payer, mint_authority.pubkey());
    let market_keypair = Keypair::new();
    let market_len = percolator_prog::state::market_account_len_for_capacity(2).unwrap();
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
                max_portfolio_assets: 2,
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
                max_bankrupt_close_lifetime_slots: 100,
                public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
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
        &[&payer, &admin, &attacker],
        pix(
            vec![
                AccountMeta::new_readonly(admin.pubkey(), true),
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::UpdateAssetAuthority {
                asset_index: ASSET,
                kind: 3,
                new_pubkey: attacker.pubkey().to_bytes(),
            },
        ),
    )
    .expect("bind the attacker as the external backing provider");
    for asset_index in [0u16, ASSET] {
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
                    asset_index,
                    kind: 4,
                    new_pubkey: oracle.pubkey().to_bytes(),
                },
            ),
        )
        .expect("bind the independent oracle");
        send(
            &mut svm,
            &[&payer, &oracle],
            pix(
                vec![
                    AccountMeta::new_readonly(oracle.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index,
                    now_slot: 0,
                    initial_mark_e6: PRICE,
                },
            ),
        )
        .expect("configure an authenticated mark");
    }

    let vault_authority = vault_authority(market);
    let vault = canonical_vault(vault_authority, collateral);
    set_token(&mut svm, vault, collateral, vault_authority, 0);
    let backing_source = Pubkey::new_unique();
    set_token(
        &mut svm,
        backing_source,
        collateral,
        attacker.pubkey(),
        BACKING as u64,
    );
    send(
        &mut svm,
        &[&payer, &attacker],
        pix(
            vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(backing_source, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::TopUpBackingBucket {
                domain: SOURCE_DOMAIN,
                amount: BACKING,
                expiry_slot: 10_000,
            },
        ),
    )
    .expect("fund the historical claim's source domain");

    let victim_portfolio = Keypair::new();
    let first_counterparty_portfolio = Keypair::new();
    let attacker_portfolio = Keypair::new();
    let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(2).unwrap();
    // Upstream #372 grows the body and relies on InitPortfolio's legacy realloc path.
    let portfolio_rent = svm.minimum_balance_for_rent_exemption(portfolio_len + 64 * 1024);
    for (owner, portfolio) in [
        (&victim, &victim_portfolio),
        (&first_counterparty, &first_counterparty_portfolio),
        (&attacker, &attacker_portfolio),
    ] {
        send(
            &mut svm,
            &[&payer, portfolio],
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
        let source = Pubkey::new_unique();
        set_token(&mut svm, source, collateral, owner.pubkey(), CAPITAL as u64);
        send(
            &mut svm,
            &[&payer, owner],
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio.pubkey(), false),
                    AccountMeta::new(source, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Deposit { amount: CAPITAL },
            ),
        )
        .expect("deposit independent trader collateral");
    }
    let victim_portfolio = victim_portfolio.pubkey();
    let first_counterparty_portfolio = first_counterparty_portfolio.pubkey();
    let attacker_portfolio = attacker_portfolio.pubkey();

    let trade = |svm: &mut LiteSVM,
                 owner_a: &Keypair,
                 portfolio_a: Pubkey,
                 owner_b: &Keypair,
                 portfolio_b: Pubkey,
                 size_q: i128,
                 price: u64| {
        send(
            svm,
            &[&payer, owner_a, owner_b],
            pix(
                vec![
                    AccountMeta::new_readonly(owner_a.pubkey(), true),
                    AccountMeta::new_readonly(owner_b.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio_a, false),
                    AccountMeta::new(portfolio_b, false),
                ],
                PIx::TradeNoCpi {
                    asset_index: ASSET,
                    size_q,
                    exec_price: price,
                    fee_bps: 0,
                },
            ),
        )
    };
    let crank = |svm: &mut LiteSVM, portfolio: Pubkey, slot: u64| {
        send(
            svm,
            &[&payer],
            pix(
                vec![
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
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
    };

    trade(
        &mut svm,
        &first_counterparty,
        first_counterparty_portfolio,
        &victim,
        victim_portfolio,
        percolator::POS_SCALE as i128,
        PRICE,
    )
    .expect("open the first episode");
    warp_to(&mut svm, 20);
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
                now_slot: 20,
                mark_e6: PROFIT_MARK,
            },
        ),
    )
    .expect("publish the first episode's profit mark");
    crank(&mut svm, victim_portfolio, 20).expect("materialize the historical claim");
    trade(
        &mut svm,
        &first_counterparty,
        first_counterparty_portfolio,
        &victim,
        victim_portfolio,
        -(percolator::POS_SCALE as i128),
        PROFIT_MARK,
    )
    .expect("fully close the first episode");
    let after_first =
        percolator_prog::state::read_portfolio(&svm.get_account(&victim_portfolio).unwrap().data)
            .unwrap();
    let historical_pnl = after_first.pnl.get();
    let historical_claim =
        percolator_prog::state::portfolio_source_domain(&after_first, SOURCE_DOMAIN as usize)
            .source_claim_bound_num
            .get();
    let counterparty_after_first = percolator_prog::state::read_portfolio(
        &svm.get_account(&first_counterparty_portfolio).unwrap().data,
    )
    .unwrap();
    assert_eq!(historical_pnl, 48_000);
    assert_eq!(historical_claim, 48_000 * percolator::BOUND_SCALE);
    assert_eq!(
        counterparty_after_first.capital.get() as i128 + counterparty_after_first.pnl.get(),
        CAPITAL as i128 - historical_pnl,
        "the independent counterparty bears the settled loss"
    );
    assert!(!has_active_leg(&svm, victim_portfolio, ASSET as usize));

    let victim_early_destination = Pubkey::new_unique();
    set_token(
        &mut svm,
        victim_early_destination,
        collateral,
        victim.pubkey(),
        0,
    );
    send(
        &mut svm,
        &[&payer, &victim],
        pix(
            vec![
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(victim_portfolio, false),
                AccountMeta::new(victim_early_destination, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::Withdraw {
                amount: CAPITAL - RETAINED_CAPITAL,
            },
        ),
    )
    .expect("retain only the collateral needed to exercise the claim-overlap branch");
    assert_eq!(
        token_amount(&svm, victim_early_destination),
        (CAPITAL - RETAINED_CAPITAL) as u64
    );

    trade(
        &mut svm,
        &attacker,
        attacker_portfolio,
        &victim,
        victim_portfolio,
        percolator::POS_SCALE as i128,
        PROFIT_MARK,
    )
    .expect("reopen the victim's same side at the current mark");
    warp_to(&mut svm, 40);
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
                now_slot: 40,
                mark_e6: SECOND_PROFIT_MARK,
            },
        ),
    )
    .expect("publish the second episode's profit mark");
    crank(&mut svm, victim_portfolio, 40).expect("materialize the second same-domain claim");
    crank(&mut svm, attacker_portfolio, 40).expect("settle the second counterparty");
    trade(
        &mut svm,
        &attacker,
        attacker_portfolio,
        &victim,
        victim_portfolio,
        (percolator::POS_SCALE / 2) as i128,
        SECOND_PROFIT_MARK,
    )
    .expect("increase risk until the new lien overlaps the historical claim floor");
    let before_shutdown =
        percolator_prog::state::read_portfolio(&svm.get_account(&victim_portfolio).unwrap().data)
            .unwrap();
    assert!(
        before_shutdown.pnl.get() > historical_pnl,
        "episode two must add a same-domain claim suffix"
    );
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
                now_slot: 40,
                initial_price: 0,
                insurance_authority: [0u8; 32],
                insurance_operator: [0u8; 32],
                backing_bucket_authority: [0u8; 32],
                oracle_authority: [0u8; 32],
            },
        ),
    )
    .expect("normal administration shuts down the reopened asset");
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
                b_delta_budget: u128::MAX,
            },
        ),
    )
    .expect("the malicious provider removes the only opposite leg");
    assert!(!has_active_leg(&svm, attacker_portfolio, ASSET as usize));

    send(
        &mut svm,
        &[&payer, &victim],
        pix(
            vec![
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(victim_portfolio, false),
            ],
            PIx::ForfeitRecoveryLeg {
                asset_index: ASSET,
                b_delta_budget: u128::MAX,
            },
        ),
    )
    .expect("the victim must retain a bounded public exit");
    let after_forfeit =
        percolator_prog::state::read_portfolio(&svm.get_account(&victim_portfolio).unwrap().data)
            .unwrap();
    assert!(!has_active_leg(&svm, victim_portfolio, ASSET as usize));
    let mode = percolator_prog::state::read_market_config_mode_and_capacity(
        &svm.get_account(&market).unwrap().data,
    )
    .unwrap()
    .1;
    assert_eq!(mode, percolator::MarketModeV16::Live);

    warp_to(&mut svm, 1_000);
    send(
        &mut svm,
        &[&payer, &oracle],
        pix(
            vec![
                AccountMeta::new_readonly(oracle.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::PushAuthMark {
                asset_index: 0,
                now_slot: 1_000,
                mark_e6: PRICE,
            },
        ),
    )
    .expect("publish a fresh base mark");
    send(
        &mut svm,
        &[&payer],
        pix(
            vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(first_counterparty_portfolio, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: 1_000,
                observations: vec![percolator_prog::ix::CrankObservationHint {
                    asset_index: 0,
                    oracle_accounts: 0,
                }],
            },
        ),
    )
    .expect("an honest keeper keeps the unrelated base asset live");
    let stale_resolve = send(
        &mut svm,
        &[&payer],
        pix(
            vec![AccountMeta::new(market, false)],
            PIx::ResolveStalePermissionless { now_slot: 1_000 },
        ),
    );
    assert!(
        stale_resolve.is_err(),
        "a current base asset must keep global resolution unavailable"
    );

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
                now_slot: 1_000,
                observations: vec![percolator_prog::ix::CrankObservationHint {
                    asset_index: 0,
                    oracle_accounts: 0,
                }],
            },
        ),
    )
    .expect("refresh the attacker's flat trading portfolio");
    let attacker_trading_capital =
        percolator_prog::state::read_portfolio(&svm.get_account(&attacker_portfolio).unwrap().data)
            .unwrap()
            .capital
            .get();
    let attacker_trading_destination = Pubkey::new_unique();
    set_token(
        &mut svm,
        attacker_trading_destination,
        collateral,
        attacker.pubkey(),
        0,
    );
    send(
        &mut svm,
        &[&payer, &attacker],
        pix(
            vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(attacker_portfolio, false),
                AccountMeta::new(attacker_trading_destination, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::Withdraw {
                amount: attacker_trading_capital,
            },
        ),
    )
    .expect("withdraw the attacker's remaining trading collateral");
    let attacker_trading_received = token_amount(&svm, attacker_trading_destination);

    let market_before_refresh = svm.get_account(&market).unwrap();
    let victim_before_refresh = svm.get_account(&victim_portfolio).unwrap();
    let vault_before_refresh = svm.get_account(&vault).unwrap();
    let refresh = send(
        &mut svm,
        &[&payer],
        pix(
            vec![
                AccountMeta::new_readonly(payer.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(victim_portfolio, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: 1_000,
                observations: vec![percolator_prog::ix::CrankObservationHint {
                    asset_index: 0,
                    oracle_accounts: 0,
                }],
            },
        ),
    );
    if refresh.is_err() {
        assert_eq!(svm.get_account(&market).unwrap(), market_before_refresh);
        assert_eq!(
            svm.get_account(&victim_portfolio).unwrap(),
            victim_before_refresh
        );
        assert_eq!(svm.get_account(&vault).unwrap(), vault_before_refresh);
    }
    let convert = send(
        &mut svm,
        &[&payer, &victim],
        pix(
            vec![
                AccountMeta::new_readonly(victim.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(victim_portfolio, false),
            ],
            PIx::ConvertReleasedPnl { amount: u128::MAX },
        ),
    );

    let backing_before_withdraw = percolator_accounting::read_asset_backing_balances(
        &svm.get_account(&market).unwrap().data,
        ASSET as usize,
    )
    .unwrap()[0];
    let attacker_destination = Pubkey::new_unique();
    set_token(
        &mut svm,
        attacker_destination,
        collateral,
        attacker.pubkey(),
        0,
    );
    let backing_withdraw = send(
        &mut svm,
        &[&payer, &attacker],
        pix(
            vec![
                AccountMeta::new_readonly(attacker.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(attacker_destination, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::WithdrawBackingBucket {
                domain: SOURCE_DOMAIN,
                amount: backing_before_withdraw.principal_atoms,
            },
        ),
    );
    let attacker_extracted = token_amount(&svm, attacker_destination);

    let victim_destination = Pubkey::new_unique();
    set_token(&mut svm, victim_destination, collateral, victim.pubkey(), 0);
    let expected_victim_withdrawal = RETAINED_CAPITAL + historical_pnl as u128;
    let full_withdraw = send(
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
            PIx::Withdraw {
                amount: expected_victim_withdrawal,
            },
        ),
    );
    let deposit_only_withdraw = if full_withdraw.is_err() {
        Some(send(
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
                PIx::Withdraw {
                    amount: RETAINED_CAPITAL,
                },
            ),
        ))
    } else {
        None
    };
    let victim_received = token_amount(&svm, victim_destination);
    assert_eq!(
        token_amount(&svm, vault)
            + attacker_extracted
            + attacker_trading_received
            + victim_received
            + token_amount(&svm, victim_early_destination),
        (3 * CAPITAL + BACKING) as u64,
        "all successful withdrawals must conserve canonical custody"
    );
    assert!(
        backing_withdraw.is_ok(),
        "the bounded provider exit remains blocked by upstream percolator-prog#371: refresh={refresh:?}, convert={convert:?}, withdraw={backing_withdraw:?}"
    );
    assert_eq!(
        attacker_extracted + attacker_trading_received,
        (CAPITAL + BACKING) as u64,
        "the attacker extracted the independent counterparty's {historical_pnl}-atom loss while the victim's prior claim remained unredeemable: backing={attacker_extracted}, trading={attacker_trading_received}, after_forfeit_pnl={}, after_forfeit_claim={}, refresh={refresh:?}, convert={convert:?}, full_withdraw={full_withdraw:?}, deposit_only_withdraw={deposit_only_withdraw:?}, victim_received={victim_received}",
        after_forfeit.pnl.get(),
        percolator_prog::state::portfolio_source_domain(
            &after_forfeit,
            SOURCE_DOMAIN as usize,
        )
        .source_claim_bound_num
        .get(),
    );
    assert_eq!(
        victim_received, expected_victim_withdrawal as u64,
        "the settled historical profit must remain redeemable after same-domain Recovery"
    );
}
