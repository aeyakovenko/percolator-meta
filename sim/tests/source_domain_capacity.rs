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

struct CapacityEnv {
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

impl CapacityEnv {
    fn new() -> Self {
        const MARKET_CAPACITY: usize = 70;
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
        svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
        svm.airdrop(&admin.pubkey(), 1_000_000_000).unwrap();
        svm.airdrop(&oracle.pubkey(), 1_000_000_000).unwrap();
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
                data: vec![
                    0;
                    percolator_prog::state::market_account_len_for_capacity(MARKET_CAPACITY)
                        .unwrap()
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
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(
                percolator_prog::constants::WRAPPER_MAX_PORTFOLIO_ASSETS as usize,
            )
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
                    max_portfolio_assets: percolator_prog::constants::WRAPPER_MAX_PORTFOLIO_ASSETS,
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
            &[&admin],
        )
        .expect("initialize capacity market");
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

    fn activate_asset(&mut self, asset_index: u16, now_slot: u64, price: u64) {
        self.set_slot(now_slot);
        let admin = self.admin.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::UpdateAssetLifecycle {
                    action: percolator_prog::processor::ASSET_ACTION_ACTIVATE,
                    asset_index,
                    now_slot,
                    initial_price: price,
                    insurance_authority: admin.pubkey().to_bytes(),
                    insurance_operator: admin.pubkey().to_bytes(),
                    backing_bucket_authority: admin.pubkey().to_bytes(),
                    oracle_authority: admin.pubkey().to_bytes(),
                },
            ),
            &[&admin],
        )
        .expect("activate asset");
    }

    fn configure_auth_mark(&mut self, asset_index: u16, now_slot: u64, price: u64) {
        let admin = self.admin.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index,
                    now_slot,
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
        .expect("separate honest oracle from portfolio owners");
    }

    fn push_mark(&mut self, asset_index: u16, now_slot: u64, price: u64) {
        let oracle = self.oracle.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(oracle.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::PushAuthMark {
                    asset_index,
                    now_slot,
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
        .expect("deposit collateral");
        (portfolio, collateral_account)
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

    fn crank(&mut self, portfolio: Pubkey, asset_index: u16, now_slot: u64) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(self.payer.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::PermissionlessCrank {
                    now_slot,
                    observations: vec![percolator_prog::ix::CrankObservationHint {
                        asset_index,
                        oracle_accounts: 0,
                    }],
                },
            ),
            &[],
        )
    }

    fn capital(&self, portfolio: Pubkey) -> u128 {
        percolator_prog::state::read_portfolio(&self.svm.get_account(&portfolio).unwrap().data)
            .unwrap()
            .capital
            .get()
    }

    fn withdraw(
        &mut self,
        owner: &Keypair,
        portfolio: Pubkey,
        destination: Pubkey,
        amount: u128,
    ) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(self.vault, false),
                    AccountMeta::new_readonly(self.vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::Withdraw { amount },
            ),
            &[owner],
        )
    }
}

// A full historical source table must reserve enough capacity to settle every admitted live leg.
// Otherwise one honest favorable mark makes the owner's position and principal permanently stuck.
#[test]
fn full_source_table_rejects_leg_that_cannot_be_settled() {
    const ACTIVE_CAP: u16 = percolator_prog::constants::WRAPPER_MAX_PORTFOLIO_ASSETS;
    const HISTORICAL_ASSETS: u16 = 16;
    const NEW_ASSET: u16 = HISTORICAL_ASSETS;
    const LOW: u64 = 100;
    const HIGH: u64 = 101;
    const DEPOSIT: u64 = 1_000_000;
    const Q: i128 = percolator::POS_SCALE as i128;

    assert_eq!(ACTIVE_CAP, 14);
    assert_eq!(percolator::PORTFOLIO_SOURCE_DOMAIN_CAP, 32);
    let mut env = CapacityEnv::new();

    for asset_index in ACTIVE_CAP..=NEW_ASSET {
        let activation_slot = u64::from(asset_index - ACTIVE_CAP + 1);
        env.activate_asset(asset_index, activation_slot, LOW);
    }
    let mut slot = u64::from(NEW_ASSET - ACTIVE_CAP + 1);
    env.set_slot(slot);
    for asset_index in 0..=NEW_ASSET {
        env.configure_auth_mark(asset_index, slot, LOW);
        env.rotate_oracle(asset_index);
    }

    let owner = Keypair::new();
    let counterparty = Keypair::new();
    let (portfolio, owner_collateral) = env.create_portfolio(&owner, DEPOSIT);
    let (counterparty_portfolio, _) = env.create_portfolio(&counterparty, DEPOSIT);

    for asset_index in 0..HISTORICAL_ASSETS {
        env.trade(
            &owner,
            portfolio,
            &counterparty,
            counterparty_portfolio,
            asset_index,
            Q,
            LOW,
        )
        .expect("open historical long");
        slot += 1;
        env.set_slot(slot);
        env.push_mark(asset_index, slot, HIGH);
        env.crank(counterparty_portfolio, asset_index, slot)
            .expect("settle losing historical short");
        env.crank(portfolio, asset_index, slot)
            .expect("settle winning historical long");
        env.trade(
            &owner,
            portfolio,
            &counterparty,
            counterparty_portfolio,
            asset_index,
            -Q,
            HIGH,
        )
        .expect("close historical long");

        env.trade(
            &owner,
            portfolio,
            &counterparty,
            counterparty_portfolio,
            asset_index,
            -Q,
            HIGH,
        )
        .expect("open historical short");
        slot += 1;
        env.set_slot(slot);
        env.push_mark(asset_index, slot, LOW);
        env.crank(counterparty_portfolio, asset_index, slot)
            .expect("settle losing historical long");
        env.crank(portfolio, asset_index, slot)
            .expect("settle winning historical short");
        env.trade(
            &owner,
            portfolio,
            &counterparty,
            counterparty_portfolio,
            asset_index,
            Q,
            LOW,
        )
        .expect("close historical short");
    }

    let portfolio_data = env.svm.get_account(&portfolio).unwrap().data;
    let filled = percolator_prog::state::read_portfolio(&portfolio_data).unwrap();
    assert_eq!(
        filled
            .source_domains
            .iter()
            .filter(|domain| domain.is_occupied())
            .count(),
        percolator::PORTFOLIO_SOURCE_DOMAIN_CAP,
        "ordinary two-sided settlements fill the complete source table"
    );
    assert_eq!(filled.pnl.get(), i128::from(2 * HISTORICAL_ASSETS));

    env.trade(
        &owner,
        portfolio,
        &counterparty,
        counterparty_portfolio,
        0,
        Q,
        LOW,
    )
    .expect("retain an already-admitted old exposure");
    let market_before_admission = env.svm.get_account(&env.market).unwrap();
    let portfolio_before_admission = env.svm.get_account(&portfolio).unwrap();
    let counterparty_before_admission = env.svm.get_account(&counterparty_portfolio).unwrap();
    let admission = env.trade(
        &owner,
        portfolio,
        &counterparty,
        counterparty_portfolio,
        NEW_ASSET,
        Q,
        LOW,
    );

    if admission.is_ok() {
        slot += 1;
        env.set_slot(slot);
        env.push_mark(NEW_ASSET, slot, HIGH);
        env.crank(counterparty_portfolio, NEW_ASSET, slot)
            .expect("losing side can publish the honest mark");

        let rescuer = Keypair::new();
        let (rescuer_portfolio, _) = env.create_portfolio(&rescuer, DEPOSIT);
        let trapped_market = env.svm.get_account(&env.market).unwrap();
        let trapped_portfolio = env.svm.get_account(&portfolio).unwrap();
        let trapped_counterparty = env.svm.get_account(&counterparty_portfolio).unwrap();
        let trapped_rescuer = env.svm.get_account(&rescuer_portfolio).unwrap();
        let trapped_vault = env.svm.get_account(&env.vault).unwrap();

        assert!(
            env.crank(portfolio, NEW_ASSET, slot).is_err(),
            "owner crank must expose the missing settlement domain"
        );
        let owner_clone = owner.insecure_clone();
        assert!(
            env.send(
                pix(
                    vec![
                        AccountMeta::new(owner_clone.pubkey(), true),
                        AccountMeta::new(env.market, false),
                        AccountMeta::new(portfolio, false),
                    ],
                    PIx::ConvertReleasedPnl { amount: 1 },
                ),
                &[&owner_clone],
            )
            .is_err(),
            "claim conversion cannot free capacity while exposure remains"
        );
        assert!(
            env.trade(
                &owner,
                portfolio,
                &rescuer,
                rescuer_portfolio,
                NEW_ASSET,
                -Q,
                HIGH,
            )
            .is_err(),
            "fresh willing counterparty cannot close the new leg"
        );
        assert!(
            env.trade(&owner, portfolio, &rescuer, rescuer_portfolio, 0, -Q, LOW,)
                .is_err(),
            "fresh willing counterparty cannot close the old leg"
        );
        let owner_clone = owner.insecure_clone();
        assert!(
            env.send(
                pix(
                    vec![
                        AccountMeta::new(owner_clone.pubkey(), true),
                        AccountMeta::new(env.market, false),
                        AccountMeta::new(portfolio, false),
                    ],
                    PIx::RebalanceReduce {
                        asset_index: NEW_ASSET,
                        reduce_q: percolator::POS_SCALE,
                    },
                ),
                &[&owner_clone],
            )
            .is_err(),
            "unilateral reduction cannot clear the trapped leg"
        );
        assert!(
            env.withdraw(&owner, portfolio, owner_collateral, 1)
                .is_err(),
            "owner principal remains non-withdrawable"
        );
        assert_eq!(env.svm.get_account(&env.market).unwrap(), trapped_market);
        assert_eq!(env.svm.get_account(&portfolio).unwrap(), trapped_portfolio);
        assert_eq!(
            env.svm.get_account(&counterparty_portfolio).unwrap(),
            trapped_counterparty
        );
        assert_eq!(
            env.svm.get_account(&rescuer_portfolio).unwrap(),
            trapped_rescuer
        );
        assert_eq!(env.svm.get_account(&env.vault).unwrap(), trapped_vault);
        panic!(
            "an admitted live leg has no bounded owner, keeper, or fresh-counterparty continuation"
        );
    }

    assert_eq!(
        env.svm.get_account(&env.market).unwrap(),
        market_before_admission,
        "unsafe admission rejects without mutating the market"
    );
    assert_eq!(
        env.svm.get_account(&portfolio).unwrap(),
        portfolio_before_admission,
        "unsafe admission rejects without mutating the full source table"
    );
    assert_eq!(
        env.svm.get_account(&counterparty_portfolio).unwrap(),
        counterparty_before_admission,
        "unsafe admission rejects without mutating its counterparty"
    );

    env.trade(
        &owner,
        portfolio,
        &counterparty,
        counterparty_portfolio,
        0,
        -Q,
        LOW,
    )
    .expect("previously admitted position remains closeable");
    let withdrawable = env.capital(portfolio);
    assert_eq!(withdrawable, u128::from(DEPOSIT));
    env.withdraw(&owner, portfolio, owner_collateral, withdrawable)
        .expect("owner withdraws principal after safe rejection");
    assert_eq!(token_amount(&env.svm, owner_collateral), DEPOSIT);
}
