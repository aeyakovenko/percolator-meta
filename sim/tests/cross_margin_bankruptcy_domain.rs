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

fn create_real_mint(svm: &mut LiteSVM, payer: &Keypair, authority: Pubkey) -> Pubkey {
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

struct BankruptcyEnv {
    svm: LiteSVM,
    payer: Keypair,
    admin: Keypair,
    oracle: Keypair,
    market: Pubkey,
    collateral: Pubkey,
    vault: Pubkey,
    vault_authority: Pubkey,
    portfolio_len: usize,
}

impl BankruptcyEnv {
    fn new() -> Self {
        const PRICE: u64 = 100;
        const ASSETS: usize = 2;

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
        svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
        svm.airdrop(&admin.pubkey(), 1_000_000_000).unwrap();
        svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
        svm.set_sysvar(&Clock {
            slot: 1,
            unix_timestamp: 1,
            ..Clock::default()
        });

        let collateral = create_real_mint(&mut svm, &payer, admin.pubkey());
        let market = Pubkey::new_unique();
        svm.set_account(
            market,
            Account {
                lamports: 1_000_000_000,
                data: vec![
                    0;
                    percolator_prog::state::market_account_len_for_capacity(ASSETS).unwrap()
                ],
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
            market,
            collateral,
            vault,
            vault_authority,
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(ASSETS)
                .unwrap(),
        };
        let admin = env.admin.insecure_clone();
        env.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(market, false),
                    AccountMeta::new_readonly(collateral, false),
                ],
                PIx::InitMarket {
                    max_portfolio_assets: ASSETS as u16,
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
                    maintenance_fee_per_slot: 200,
                },
            ),
            &[&admin],
        )
        .expect("initialize cross-domain market");
        for asset_index in [0, 1] {
            env.configure_auth_mark(asset_index, PRICE);
            env.rotate_oracle(asset_index);
        }
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

    fn configure_auth_mark(&mut self, asset_index: u16, price: u64) {
        let admin = self.admin.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index,
                    now_slot: 1,
                    initial_mark_e6: price,
                },
            ),
            &[&admin],
        )
        .expect("configure authenticated mark");
    }

    fn rotate_oracle(&mut self, asset_index: u16) {
        let admin = self.admin.insecure_clone();
        let oracle = self.oracle.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(oracle.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::UpdateAssetAuthority {
                    asset_index,
                    kind: percolator_prog::processor::ASSET_AUTH_ORACLE,
                    new_pubkey: oracle.pubkey().to_bytes(),
                },
            ),
            &[&admin, &oracle],
        )
        .expect("separate honest oracle");
    }

    fn rotate_insurance_authority(&mut self, asset_index: u16, provider: &Keypair) {
        let admin = self.admin.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::UpdateAssetAuthority {
                    asset_index,
                    kind: percolator_prog::processor::ASSET_AUTH_INSURANCE,
                    new_pubkey: provider.pubkey().to_bytes(),
                },
            ),
            &[&admin, provider],
        )
        .expect("install independent insurance provider");
    }

    fn push_mark(&mut self, asset_index: u16, slot: u64, price: u64) {
        let oracle = self.oracle.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(oracle.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::PushAuthMark {
                    asset_index,
                    now_slot: slot,
                    mark_e6: price,
                },
            ),
            &[&oracle],
        )
        .expect("honest oracle mark");
    }

    fn create_portfolio(&mut self, owner: &Keypair, deposit: u64) -> (Pubkey, Pubkey) {
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

        let collateral_account = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            collateral_account,
            self.collateral,
            owner.pubkey(),
            deposit,
        );
        if deposit != 0 {
            self.send(
                pix(
                    vec![
                        AccountMeta::new(owner.pubkey(), true),
                        AccountMeta::new(self.market, false),
                        AccountMeta::new(portfolio, false),
                        AccountMeta::new(collateral_account, false),
                        AccountMeta::new(self.vault, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    PIx::Deposit {
                        amount: u128::from(deposit),
                    },
                ),
                &[owner],
            )
            .expect("deposit portfolio collateral");
        }
        (portfolio, collateral_account)
    }

    fn insurance_ledger(&mut self) -> Pubkey {
        let ledger = Pubkey::new_unique();
        self.svm
            .set_account(
                ledger,
                Account {
                    lamports: 1_000_000_000,
                    data: vec![0; percolator_prog::state::insurance_ledger_account_len()],
                    owner: perc_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        ledger
    }

    fn top_up_insurance(
        &mut self,
        provider: &Keypair,
        ledger: Pubkey,
        domain: u16,
        amount: u64,
    ) -> Pubkey {
        self.svm.airdrop(&provider.pubkey(), 1_000_000_000).unwrap();
        let provider_source = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            provider_source,
            self.collateral,
            provider.pubkey(),
            amount,
        );
        self.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(provider_source, false),
                    AccountMeta::new(self.vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                    AccountMeta::new(ledger, false),
                ],
                PIx::TopUpInsuranceDomain {
                    domain,
                    amount: u128::from(amount),
                },
            ),
            &[provider],
        )
        .expect("independent provider funds one insurance domain through its ledger");
        provider_source
    }

    fn sync_insurance_ledger(&mut self, provider: &Keypair, ledger: Pubkey) {
        self.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(ledger, false),
                ],
                PIx::SyncInsuranceLedger,
            ),
            &[provider],
        )
        .expect("provider synchronizes its loss-accounting ledger");
    }

    fn try_withdraw_terminal_insurance(
        &mut self,
        provider: &Keypair,
        ledger: Pubkey,
        amount: u128,
    ) -> (Pubkey, Result<(), String>) {
        let destination = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            destination,
            self.collateral,
            provider.pubkey(),
            0,
        );
        let result = self.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(self.vault, false),
                    AccountMeta::new_readonly(self.vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                    AccountMeta::new(ledger, false),
                ],
                PIx::WithdrawInsurance { amount },
            ),
            &[provider],
        );
        (destination, result)
    }

    fn trade(
        &mut self,
        owner_a: &Keypair,
        portfolio_a: Pubkey,
        owner_b: &Keypair,
        portfolio_b: Pubkey,
        asset_index: u16,
        size_q: i128,
        price: u64,
    ) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(owner_a.pubkey(), true),
                    AccountMeta::new(owner_b.pubkey(), true),
                    AccountMeta::new(self.market, false),
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
            &[owner_a, owner_b],
        )
    }

    fn try_crank(
        &mut self,
        portfolio: Pubkey,
        slot: u64,
        observations: Vec<percolator_prog::ix::CrankObservationHint>,
    ) -> Result<(), String> {
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

    fn crank(&mut self, portfolio: Pubkey, asset_index: u16, slot: u64) {
        self.try_crank(
            portfolio,
            slot,
            vec![percolator_prog::ix::CrankObservationHint {
                asset_index,
                oracle_accounts: 0,
            }],
        )
        .expect("permissionless crank");
    }

    fn sync_maintenance_fee(&mut self, portfolio: Pubkey, slot: u64) {
        self.send(
            pix(
                vec![
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::SyncMaintenanceFee { now_slot: slot },
            ),
            &[],
        )
        .expect("public maintenance synchronization");
    }

    fn rebalance_reduce(
        &mut self,
        owner: &Keypair,
        portfolio: Pubkey,
        asset_index: u16,
        reduce_q: u128,
    ) {
        self.send(
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::RebalanceReduce {
                    asset_index,
                    reduce_q,
                },
            ),
            &[owner],
        )
        .expect("owner executes a public risk-reducing exit");
    }

    fn resolve(&mut self) {
        let admin = self.admin.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::ResolveMarket,
            ),
            &[&admin],
        )
        .expect("bounded lifecycle authority resolves the market");
    }

    fn portfolio(&self, portfolio: Pubkey) -> percolator::PortfolioAccountV16Account {
        percolator_prog::state::read_portfolio(&self.svm.get_account(&portfolio).unwrap().data)
            .unwrap()
    }

    fn group(&self) -> percolator_prog::state::MarketGroupV16 {
        percolator_prog::state::read_market(&self.svm.get_account(&self.market).unwrap().data)
            .unwrap()
            .1
    }

    fn has_active_leg(&self, portfolio: Pubkey, asset_index: usize) -> bool {
        self.portfolio(portfolio)
            .legs
            .iter()
            .filter_map(|leg| leg.try_to_runtime().ok())
            .any(|leg| leg.active && leg.asset_index == asset_index as u32)
    }

    fn close_to_terminal(&mut self, label: &str, owner: &Keypair, portfolio: Pubkey) -> u128 {
        let mut payout = 0u128;
        for _ in 0..512 {
            let account = self.portfolio(portfolio);
            let receipt = account.resolved_payout_receipt.try_to_runtime().unwrap();
            let terminal = percolator::active_bitmap_is_empty(
                account.active_bitmap.map(percolator::V16PodU64::get),
            ) && account.capital.get() == 0
                && account.pnl.get() == 0
                && (!receipt.present || receipt.finalized);
            if terminal {
                self.send(
                    pix(
                        vec![
                            AccountMeta::new(owner.pubkey(), true),
                            AccountMeta::new(self.market, false),
                            AccountMeta::new(portfolio, false),
                        ],
                        PIx::ClosePortfolio,
                    ),
                    &[owner],
                )
                .expect("owner deregisters its terminal portfolio");
                return payout;
            }

            let destination = Pubkey::new_unique();
            set_token(
                &mut self.svm,
                destination,
                self.collateral,
                owner.pubkey(),
                0,
            );
            self.send(
                pix(
                    vec![
                        AccountMeta::new_readonly(owner.pubkey(), false),
                        AccountMeta::new(self.market, false),
                        AccountMeta::new(portfolio, false),
                        AccountMeta::new(destination, false),
                        AccountMeta::new(self.vault, false),
                        AccountMeta::new_readonly(self.vault_authority, false),
                        AccountMeta::new_readonly(spl_token::ID, false),
                    ],
                    PIx::CloseResolved {
                        fee_rate_per_slot: 0,
                    },
                ),
                &[],
            )
            .expect("resolved portfolio makes bounded close progress");
            payout += u128::from(token_amount(&self.svm, destination));
        }
        let account = self.portfolio(portfolio);
        let receipt = account.resolved_payout_receipt.try_to_runtime().unwrap();
        panic!(
            "{label} did not close: capital={} pnl={} active={} b_stale={} receipt={receipt:?}",
            account.capital.get(),
            account.pnl.get(),
            percolator::active_bitmap_count_ones(
                account.active_bitmap.map(percolator::V16PodU64::get)
            ),
            account.b_stale_state,
        );
    }
}

// A loss realized on asset 1 must retain its source domain after that leg is flattened. Otherwise a
// later liquidation can infer the domain from the unrelated surviving asset-0 leg and pay the
// attacker's asset-1 winner from an independent asset-0 insurer's segregated principal.
#[test]
fn flattened_cross_margin_loss_cannot_drain_unrelated_insurance() {
    const PRICE: u64 = 100;
    const PROVIDER_PRINCIPAL: u64 = 100_000;
    const UNRELATED_DOMAIN: usize = 1;
    const LOSER_DEPOSIT: u64 = 200;
    const WINNER_DEPOSIT: u64 = 10_000;
    const ASSET0_CP_DEPOSIT: u64 = 10_000;

    let mut env = BankruptcyEnv::new();
    let provider = Keypair::new();
    env.rotate_insurance_authority(0, &provider);
    let insurance_ledger = env.insurance_ledger();
    let provider_source = env.top_up_insurance(
        &provider,
        insurance_ledger,
        UNRELATED_DOMAIN as u16,
        PROVIDER_PRINCIPAL,
    );
    assert_eq!(token_amount(&env.svm, provider_source), 0);
    let initial_ledger = percolator_prog::state::read_insurance_ledger(
        &env.svm.get_account(&insurance_ledger).unwrap().data,
    )
    .unwrap();
    assert_eq!(initial_ledger.total_principal_atoms, 100_000);

    let loser_owner = Keypair::new();
    let winner = Keypair::new();
    let asset0_cp_owner = Keypair::new();
    let observer_owner = Keypair::new();
    let (loser, _) = env.create_portfolio(&loser_owner, LOSER_DEPOSIT);
    let (winner_portfolio, _) = env.create_portfolio(&winner, WINNER_DEPOSIT);
    let (asset0_cp, _) = env.create_portfolio(&asset0_cp_owner, ASSET0_CP_DEPOSIT);
    let (observer, _) = env.create_portfolio(&observer_owner, 0);

    env.trade(
        &loser_owner,
        loser,
        &asset0_cp_owner,
        asset0_cp,
        0,
        percolator::POS_SCALE as i128,
        PRICE,
    )
    .expect("open unrelated asset-0 exposure");
    env.trade(
        &loser_owner,
        loser,
        &winner,
        winner_portfolio,
        1,
        -(percolator::POS_SCALE as i128),
        PRICE,
    )
    .expect("open the loss-bearing asset-1 exposure");

    env.set_slot(2);
    for asset_index in [0, 1] {
        env.crank(observer, asset_index, 2);
    }
    env.sync_maintenance_fee(loser, 2);
    assert_eq!(env.portfolio(loser).capital.get(), 0);

    let mut mark = PRICE;
    for slot in 3..=12 {
        mark = mark.checked_mul(2).unwrap();
        env.set_slot(slot);
        env.push_mark(1, slot, mark);
        env.crank(observer, 1, slot);
    }
    env.rebalance_reduce(&loser_owner, loser, 1, percolator::POS_SCALE);
    assert!(!env.has_active_leg(loser, 1));
    assert!(env.has_active_leg(loser, 0));
    assert!(env.portfolio(loser).pnl.get() < 0);

    let before = env.group();
    let mut liquidation_calls = 0u64;
    loop {
        let state = env.portfolio(loser);
        if state.pnl.get() >= 0 && !env.has_active_leg(loser, 0) {
            break;
        }
        assert!(liquidation_calls < 512, "bounded liquidation must progress");
        match env.try_crank(loser, 12, vec![]) {
            Ok(()) => liquidation_calls += 1,
            Err(error) => {
                assert!(
                    error.contains("Custom(23)"),
                    "only rollback-safe RecoveryRequired is accepted: {error}"
                );
                break;
            }
        }
    }

    let after = env.group();
    let unrelated_spend = after.insurance_domain_spent[UNRELATED_DOMAIN]
        .checked_sub(before.insurance_domain_spent[UNRELATED_DOMAIN])
        .unwrap();
    assert_eq!(
        after.insurance_domain_spent[0],
        before.insurance_domain_spent[0]
    );
    assert_eq!(
        after.insurance_domain_spent[2],
        before.insurance_domain_spent[2]
    );
    assert_eq!(
        after.insurance_domain_spent[3],
        before.insurance_domain_spent[3]
    );

    env.sync_insurance_ledger(&provider, insurance_ledger);
    let loss_ledger = percolator_prog::state::read_insurance_ledger(
        &env.svm.get_account(&insurance_ledger).unwrap().data,
    )
    .unwrap();
    let preclose_recoverable_principal = loss_ledger.total_principal_atoms;

    env.resolve();
    let loser_payout = env.close_to_terminal("loser", &loser_owner, loser);
    let asset0_cp_payout =
        env.close_to_terminal("asset-0 counterparty", &asset0_cp_owner, asset0_cp);
    let winner_payout = env.close_to_terminal("asset-1 winner", &winner, winner_portfolio);
    assert_eq!(
        env.close_to_terminal("observer", &observer_owner, observer),
        0
    );
    env.sync_insurance_ledger(&provider, insurance_ledger);
    let terminal_ledger = percolator_prog::state::read_insurance_ledger(
        &env.svm.get_account(&insurance_ledger).unwrap().data,
    )
    .unwrap();
    let terminal_group = env.group();
    let asset0_remaining = [0usize, 1]
        .into_iter()
        .map(|domain| {
            terminal_group.insurance_domain_budget[domain]
                .saturating_sub(terminal_group.insurance_domain_spent[domain])
        })
        .sum::<u128>();
    let recoverable_principal = terminal_ledger.total_principal_atoms.min(asset0_remaining);

    let attacker_payout = loser_payout + asset0_cp_payout + winner_payout;
    let attacker_deposit = u128::from(LOSER_DEPOSIT + WINNER_DEPOSIT + ASSET0_CP_DEPOSIT);
    let attacker_gain = attacker_payout.saturating_sub(attacker_deposit);

    let provider_withdraw_amount = recoverable_principal.max(1);
    let (provider_destination, provider_withdrawal) =
        env.try_withdraw_terminal_insurance(&provider, insurance_ledger, provider_withdraw_amount);
    let provider_recovered = u128::from(token_amount(&env.svm, provider_destination));
    if recoverable_principal == 0 {
        assert!(provider_withdrawal.is_err());
        assert_eq!(provider_recovered, 0);
    } else {
        provider_withdrawal.expect("unspent provider principal remains withdrawable");
        assert_eq!(provider_recovered, recoverable_principal);
    }
    let provider_loss = u128::from(PROVIDER_PRINCIPAL) - provider_recovered;

    if unrelated_spend != 0 {
        assert_eq!(unrelated_spend, 100_100);
        assert_eq!(attacker_payout, 115_700);
        assert_eq!(attacker_gain, 95_500);
        assert_eq!(provider_recovered, 4_500);
        assert_eq!(provider_loss, attacker_gain);
        assert_eq!(terminal_ledger.cumulative_loss_atoms, 99_900);
    }
    assert_eq!(
        unrelated_spend, 0,
        "asset-1 debt spent {unrelated_spend} atoms from independent asset-0 insurance; \
         coalition payout={attacker_payout}, deposit={attacker_deposit}, gain={attacker_gain}, \
         provider loss={provider_loss}, preclose principal={preclose_recoverable_principal}, \
         liquidation calls={liquidation_calls}"
    );
}
