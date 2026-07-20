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

fn pix(accounts: Vec<AccountMeta>, ix: PIx) -> Instruction {
    Instruction {
        program_id: perc_id(),
        accounts,
        data: ix.encode(),
    }
}

fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
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
            data: token_account_data(&mint, &owner, amount),
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn token_amount(svm: &LiteSVM, key: Pubkey) -> u64 {
    svm.get_account(&key)
        .map(|account| u64::from_le_bytes(account.data[64..72].try_into().unwrap()))
        .unwrap_or(0)
}

fn create_real_mint(svm: &mut LiteSVM, payer: &Keypair, authority: Pubkey) -> Pubkey {
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

fn insurance(svm: &LiteSVM, market: Pubkey) -> u128 {
    let data = svm.get_account(&market).unwrap().data;
    percolator_prog::state::read_market(&data)
        .unwrap()
        .1
        .insurance
}

// The victim authorizes the large trade before any pending target exists. The attacker then pays
// for a tiny seed move. New open interest must enter after that move instead of inheriting its PnL.
#[test]
fn presigned_trade_cannot_inherit_pending_ewma_pnl() {
    const MARK: u64 = 1_000_000;
    const LARGE_Q: i128 = 1_000 * percolator::POS_SCALE as i128;
    const LARGE_DEPOSIT: u64 = 2_000_000_000;
    const SEED_DEPOSIT: u64 = 2_000_000;

    let mut svm =
        LiteSVM::new().with_compute_budget(solana_program_runtime::compute_budget::ComputeBudget {
            compute_unit_limit: 1_400_000,
            heap_size: 256 * 1024,
            ..solana_program_runtime::compute_budget::ComputeBudget::default()
        });
    svm.add_program_from_file(perc_id(), perc_so()).unwrap();

    let payer = Keypair::new();
    let admin = Keypair::new();
    let seed_long = Keypair::new();
    let seed_short = Keypair::new();
    let attacker = Keypair::new();
    let victim = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
    for signer in [&admin, &seed_long, &seed_short, &attacker, &victim] {
        svm.airdrop(&signer.pubkey(), 1_000_000_000).unwrap();
    }
    svm.set_sysvar(&Clock {
        slot: 0,
        unix_timestamp: 0,
        ..Clock::default()
    });

    let collateral = create_real_mint(&mut svm, &payer, admin.pubkey());
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
    let vault_authority = vault_authority(market);
    let vault = canonical_vault(vault_authority, collateral);
    set_token(&mut svm, vault, collateral, vault_authority, 0);

    let send = |svm: &mut LiteSVM, ix: Instruction, signers: &[&Keypair]| {
        svm.expire_blockhash();
        let mut all_signers = vec![&payer];
        all_signers.extend_from_slice(signers);
        svm.send_transaction(Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &all_signers,
            svm.latest_blockhash(),
        ))
    };

    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new_readonly(collateral, false),
            ],
            PIx::InitMarket {
                max_portfolio_assets: 1,
                h_min: 0,
                h_max: 6_480_000,
                initial_price: MARK,
                min_nonzero_mm_req: 599,
                min_nonzero_im_req: 600,
                maintenance_margin_bps: 500,
                initial_margin_bps: 500,
                max_trading_fee_bps: 100,
                trade_fee_base_bps: 0,
                liquidation_fee_bps: 0,
                liquidation_fee_cap: 0,
                min_liquidation_abs: 0,
                max_price_move_bps_per_slot: 100,
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
        &[&admin],
    )
    .expect("initialize trade-driven EWMA market");
    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::ConfigureEwmaMark {
                asset_index: 0,
                now_slot: 0,
                initial_mark_e6: MARK,
                mark_ewma_halflife_slots: 1,
                mark_min_fee: 0,
            },
        ),
        &[&admin],
    )
    .expect("configure trade-driven EWMA");

    let seed_long_portfolio = Pubkey::new_unique();
    let seed_short_portfolio = Pubkey::new_unique();
    let attacker_portfolio = Pubkey::new_unique();
    let victim_portfolio = Pubkey::new_unique();
    let portfolio_len = percolator_prog::state::portfolio_account_len_for_market_slots(1).unwrap();
    let participants = [
        (&seed_long, seed_long_portfolio, SEED_DEPOSIT),
        (&seed_short, seed_short_portfolio, SEED_DEPOSIT),
        (&attacker, attacker_portfolio, LARGE_DEPOSIT),
        (&victim, victim_portfolio, LARGE_DEPOSIT),
    ];
    let mut collateral_accounts = Vec::new();
    for (owner, portfolio, deposit) in participants {
        svm.set_account(
            portfolio,
            Account {
                lamports: 1_000_000_000,
                data: vec![0; portfolio_len],
                owner: perc_id(),
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
        send(
            &mut svm,
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::InitPortfolio,
            ),
            &[owner],
        )
        .expect("initialize public portfolio");
        let token = Pubkey::new_unique();
        set_token(&mut svm, token, collateral, owner.pubkey(), deposit);
        send(
            &mut svm,
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(token, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Deposit {
                    amount: u128::from(deposit),
                },
            ),
            &[owner],
        )
        .expect("deposit real SPL collateral");
        collateral_accounts.push(token);
    }
    let seed_long_collateral = collateral_accounts[0];
    let seed_short_collateral = collateral_accounts[1];
    let attacker_collateral = collateral_accounts[2];
    let victim_collateral = collateral_accounts[3];

    svm.set_sysvar(&Clock {
        slot: 1,
        unix_timestamp: 1,
        ..Clock::default()
    });
    let shared_blockhash = svm.latest_blockhash();
    let presigned_open = Transaction::new_signed_with_payer(
        &[pix(
            vec![
                AccountMeta::new(attacker.pubkey(), true),
                AccountMeta::new(victim.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(attacker_portfolio, false),
                AccountMeta::new(victim_portfolio, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: LARGE_Q,
                exec_price: MARK,
                fee_bps: 0,
            },
        )],
        Some(&payer.pubkey()),
        &[&payer, &attacker, &victim],
        shared_blockhash,
    );

    let portfolio_capital = |svm: &LiteSVM, portfolio: Pubkey| {
        percolator_prog::state::read_portfolio(&svm.get_account(&portfolio).unwrap().data)
            .unwrap()
            .capital
            .get()
    };
    let seed_capital_before = portfolio_capital(&svm, seed_long_portfolio)
        + portfolio_capital(&svm, seed_short_portfolio);
    let insurance_before_seed = insurance(&svm, market);
    let seed_trade = Transaction::new_signed_with_payer(
        &[pix(
            vec![
                AccountMeta::new(seed_long.pubkey(), true),
                AccountMeta::new(seed_short.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(seed_long_portfolio, false),
                AccountMeta::new(seed_short_portfolio, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 0,
                size_q: percolator::POS_SCALE as i128,
                exec_price: MARK * 2,
                fee_bps: 0,
            },
        )],
        Some(&payer.pubkey()),
        &[&payer, &seed_long, &seed_short],
        shared_blockhash,
    );
    svm.send_transaction(seed_trade)
        .expect("attacker queues paid EWMA move");
    let seed_capital_after = portfolio_capital(&svm, seed_long_portfolio)
        + portfolio_capital(&svm, seed_short_portfolio);
    let seed_cost = seed_capital_before - seed_capital_after;
    let movement_fee = insurance(&svm, market) - insurance_before_seed;
    let market_account = svm.get_account(&market).unwrap();
    let profile_after_seed =
        percolator_prog::state::read_asset_oracle_profile(&market_account.data, 0).unwrap();
    let effective_after_seed = percolator_prog::state::read_market(&market_account.data)
        .unwrap()
        .1
        .assets[0]
        .effective_price;
    assert!(seed_cost > 0, "seed trade must pay for the queued mark");
    assert_eq!(seed_cost, movement_fee);
    assert!(profile_after_seed.mark_ewma_e6 > MARK);
    assert_eq!(
        effective_after_seed, MARK,
        "queued mark must remain pending"
    );

    svm.send_transaction(presigned_open)
        .expect("unchanged victim authorization remains executable");

    let portfolios = [
        seed_long_portfolio,
        seed_short_portfolio,
        attacker_portfolio,
        victim_portfolio,
    ];
    let crank = |svm: &mut LiteSVM, portfolio: Pubkey, now_slot: u64| {
        send(
            svm,
            pix(
                vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::PermissionlessCrank {
                    now_slot,
                    observations: vec![percolator_prog::ix::CrankObservationHint {
                        asset_index: 0,
                        oracle_accounts: 0,
                    }],
                },
            ),
            &[],
        )
    };
    let mut applied_mark = MARK;
    for slot in 2..=64 {
        svm.set_sysvar(&Clock {
            slot,
            unix_timestamp: slot as i64,
            ..Clock::default()
        });
        for portfolio in portfolios {
            crank(&mut svm, portfolio, slot).expect("public EWMA crank");
        }
        let market_account = svm.get_account(&market).unwrap();
        applied_mark = percolator_prog::state::read_market(&market_account.data)
            .unwrap()
            .1
            .assets[0]
            .effective_price;
        let target = percolator_prog::state::read_asset_oracle_profile(&market_account.data, 0)
            .unwrap()
            .mark_ewma_e6;
        if applied_mark == target {
            break;
        }
    }
    assert!(applied_mark > MARK, "paid EWMA move must apply publicly");

    send(
        &mut svm,
        pix(
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market, false),
            ],
            PIx::ResolveMarket,
        ),
        &[&admin],
    )
    .expect("honest admin resolves after target convergence");

    let payout_accounts = [
        (victim.pubkey(), victim_portfolio, victim_collateral),
        (
            seed_short.pubkey(),
            seed_short_portfolio,
            seed_short_collateral,
        ),
        (attacker.pubkey(), attacker_portfolio, attacker_collateral),
        (
            seed_long.pubkey(),
            seed_long_portfolio,
            seed_long_collateral,
        ),
    ];
    let close_resolved =
        |svm: &mut LiteSVM, owner: Pubkey, portfolio: Pubkey, destination: Pubkey| {
            send(
                svm,
                pix(
                    vec![
                        AccountMeta::new_readonly(owner, false),
                        AccountMeta::new(market, false),
                        AccountMeta::new(portfolio, false),
                        AccountMeta::new(destination, false),
                        AccountMeta::new(vault, false),
                        AccountMeta::new_readonly(vault_authority, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    PIx::CloseResolved {
                        fee_rate_per_slot: 0,
                    },
                ),
                &[],
            )
        };
    let claim_topup = |svm: &mut LiteSVM, owner: Pubkey, portfolio: Pubkey, destination: Pubkey| {
        send(
            svm,
            pix(
                vec![
                    AccountMeta::new_readonly(owner, false),
                    AccountMeta::new(market, false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::ClaimResolvedPayoutTopup,
            ),
            &[],
        )
    };

    let mut terminal = false;
    for _ in 0..64 {
        for (owner, portfolio, destination) in payout_accounts {
            let _ = close_resolved(&mut svm, owner, portfolio, destination);
        }
        for (owner, portfolio, destination) in payout_accounts {
            let _ = claim_topup(&mut svm, owner, portfolio, destination);
        }
        if u128::from(token_amount(&svm, vault)) == insurance(&svm, market) {
            terminal = true;
            break;
        }
    }
    assert!(
        terminal,
        "bounded resolved waterfall must return every user atom"
    );

    let coalition_deposits = u128::from(LARGE_DEPOSIT) + 2 * u128::from(SEED_DEPOSIT);
    let coalition_withdrawn = u128::from(token_amount(&svm, attacker_collateral))
        + u128::from(token_amount(&svm, seed_long_collateral))
        + u128::from(token_amount(&svm, seed_short_collateral));
    let victim_withdrawn = u128::from(token_amount(&svm, victim_collateral));
    let retained_insurance = u128::from(token_amount(&svm, vault));
    let total_deposited = coalition_deposits + u128::from(LARGE_DEPOSIT);
    assert_eq!(
        coalition_withdrawn + victim_withdrawn + retained_insurance,
        total_deposited,
        "terminal SPL conservation"
    );
    assert!(
        coalition_withdrawn <= coalition_deposits,
        "pending EWMA paid coalition {coalition_withdrawn} from {coalition_deposits} deposited; victim withdrew {victim_withdrawn}; seed cost {seed_cost}; movement fee {movement_fee}; applied mark {applied_mark}"
    );
    assert_eq!(
        victim_withdrawn,
        u128::from(LARGE_DEPOSIT),
        "pre-authorizing the stale trade cannot make the independent victim inherit pending mark loss"
    );
}
