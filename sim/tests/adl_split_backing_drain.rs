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

struct IsolationEnv {
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

impl IsolationEnv {
    fn new() -> Self {
        const PRICE: u64 = 100;
        const ASSETS: usize = 1;

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
                    maintenance_margin_bps: 200,
                    initial_margin_bps: 200,
                    max_trading_fee_bps: 10_000,
                    trade_fee_base_bps: 0,
                    liquidation_fee_bps: 0,
                    liquidation_fee_cap: 0,
                    min_liquidation_abs: 0,
                    max_price_move_bps_per_slot: 100,
                    max_accrual_dt_slots: 1,
                    max_abs_funding_e9_per_slot: 0,
                    min_funding_lifetime_slots: 10_000_000,
                    max_account_b_settlement_chunks: 1,
                    max_bankrupt_close_chunks: 1,
                    max_bankrupt_close_lifetime_slots: 100,
                    public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                    maintenance_fee_per_slot: 500,
                },
            ),
            &[&admin],
        )
        .expect("initialize cross-domain market");
        env.configure_auth_mark(0, PRICE);
        env.rotate_oracle(0);
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

    fn rotate_backing_authority(&mut self, asset_index: u16, provider: &Keypair) {
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
                    kind: percolator_prog::processor::ASSET_AUTH_BACKING_BUCKET,
                    new_pubkey: provider.pubkey().to_bytes(),
                },
            ),
            &[&admin, provider],
        )
        .expect("install independent backing provider");
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
        (portfolio, collateral_account)
    }

    fn deposit_more(&mut self, owner: &Keypair, portfolio: Pubkey, amount: u64) {
        let collateral_account = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            collateral_account,
            self.collateral,
            owner.pubkey(),
            amount,
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
                    amount: u128::from(amount),
                },
            ),
            &[owner],
        )
        .expect("restore the partially liquidated counterparty's margin");
    }

    fn top_up_backing(
        &mut self,
        provider: &Keypair,
        domain: u16,
        amount: u64,
        expiry_slot: u64,
    ) -> Pubkey {
        self.svm.airdrop(&provider.pubkey(), 1_000_000_000).unwrap();
        let provider_collateral = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            provider_collateral,
            self.collateral,
            provider.pubkey(),
            amount,
        );
        self.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(provider_collateral, false),
                    AccountMeta::new(self.vault, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::TopUpBackingBucket {
                    domain,
                    amount: u128::from(amount),
                    expiry_slot,
                },
            ),
            &[provider],
        )
        .expect("independent provider funds one backing domain");
        provider_collateral
    }

    fn withdraw_backing(
        &mut self,
        provider: &Keypair,
        destination: Pubkey,
        domain: u16,
        amount: u128,
    ) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(self.vault, false),
                    AccountMeta::new_readonly(self.vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::WithdrawBackingBucket { domain, amount },
            ),
            &[provider],
        )
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

    fn crank(&mut self, portfolio: Pubkey, asset_index: u16, slot: u64) {
        self.try_crank(portfolio, asset_index, slot)
            .expect("permissionless crank");
    }

    fn try_crank(&mut self, portfolio: Pubkey, asset_index: u16, slot: u64) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(self.payer.pubkey(), true),
                    AccountMeta::new(self.market, false),
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
            &[],
        )
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

    fn convert(&mut self, owner: &Keypair, portfolio: Pubkey, amount: u128) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::ConvertReleasedPnl { amount },
            ),
            &[owner],
        )
    }

    fn withdraw_portfolio(
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

    fn pnl(&self, portfolio: Pubkey) -> i128 {
        self.portfolio(portfolio).pnl.get()
    }

    fn portfolio(&self, portfolio: Pubkey) -> percolator::PortfolioAccountV16Account {
        percolator_prog::state::read_portfolio(&self.svm.get_account(&portfolio).unwrap().data)
            .unwrap()
    }

    fn asset(&self) -> percolator::AssetStateV16 {
        percolator_prog::state::read_market(&self.svm.get_account(&self.market).unwrap().data)
            .unwrap()
            .1
            .assets[0]
    }

    fn basis_q(&self, portfolio: Pubkey) -> Option<i128> {
        self.portfolio(portfolio)
            .legs
            .iter()
            .filter_map(|leg| leg.try_to_runtime().ok())
            .find(|leg| leg.active && leg.asset_index == 0)
            .map(|leg| leg.basis_pos_q)
    }

    fn source_claim(&self, domain: usize) -> u128 {
        percolator_prog::state::read_market(&self.svm.get_account(&self.market).unwrap().data)
            .unwrap()
            .1
            .source_credit[domain]
            .positive_claim_bound_num
    }
}

// Quantity ADL reduces effective OI but leaves raw leg basis in place. Splitting a transfer of
// that basis must not reissue the haircut exposure into a fresh portfolio and externalize its PnL.
#[test]
fn split_post_adl_basis_cannot_drain_independent_backing() {
    const PRICE: u64 = 100;
    const PRICE_MOVE: u64 = 40;
    const OPEN_Q: i128 = 13_000 * percolator::POS_SCALE as i128;
    const PROVIDER_BACKING: u64 = 1_000_000;
    const SURVIVOR_DEPOSIT: u64 = 50_000;
    const LIQUIDATED_DEPOSIT: u64 = 30_000;
    const LIQUIDATED_TOP_UP: u64 = 500_000;
    const SUCCESSOR_DEPOSIT: u64 = 50_000;

    let mut env = IsolationEnv::new();
    let provider = Keypair::new();
    env.rotate_backing_authority(0, &provider);
    let provider_collateral = env.top_up_backing(&provider, 1, PROVIDER_BACKING, 10_000);

    let survivor_owner = Keypair::new();
    let liquidated_owner = Keypair::new();
    let successor_owner = Keypair::new();
    let (survivor, _) = env.create_portfolio(&survivor_owner, SURVIVOR_DEPOSIT);
    let (liquidated, liquidated_collateral) =
        env.create_portfolio(&liquidated_owner, LIQUIDATED_DEPOSIT);
    let (successor, successor_collateral) =
        env.create_portfolio(&successor_owner, SUCCESSOR_DEPOSIT);

    for slot in 2..=7 {
        env.set_slot(slot);
        env.crank(survivor, 0, slot);
    }
    env.set_slot(8);
    env.trade(
        &survivor_owner,
        survivor,
        &liquidated_owner,
        liquidated,
        0,
        OPEN_Q,
        PRICE,
    )
    .expect("open balanced interest");
    for slot in 9..=34 {
        env.set_slot(slot);
        env.crank(survivor, 0, slot);
    }
    env.set_slot(35);
    env.sync_maintenance_fee(liquidated, 35);
    env.crank(liquidated, 0, 35);
    env.crank(liquidated, 0, 35);
    let setup_fee_cost = u128::from(LIQUIDATED_DEPOSIT) - env.portfolio(liquidated).capital.get();
    env.deposit_more(&liquidated_owner, liquidated, LIQUIDATED_TOP_UP);

    let after_liquidation = env.asset();
    let raw_q = env.basis_q(survivor).unwrap().unsigned_abs();
    let transfer_q = after_liquidation.oi_eff_long_q;
    assert_eq!(raw_q, OPEN_Q.unsigned_abs());
    assert!(transfer_q < raw_q);
    assert!(after_liquidation.a_long < percolator::ADL_ONE);

    let first_q = transfer_q / 2;
    let second_q = transfer_q - first_q;
    let market_before_split = env.svm.get_account(&env.market).unwrap();
    let survivor_before_split = env.svm.get_account(&survivor).unwrap();
    let successor_before_split = env.svm.get_account(&successor).unwrap();
    let vault_before_split = env.svm.get_account(&env.vault).unwrap();
    let first_split = env.trade(
        &survivor_owner,
        survivor,
        &successor_owner,
        successor,
        0,
        -(first_q as i128),
        PRICE,
    );
    if let Err(rejected) = first_split {
        assert!(
            rejected.contains("Custom(21)") || rejected.contains("custom program error: 0x15"),
            "fixed path must reject fresh post-ADL risk as LockActive, got {rejected}"
        );
        assert_eq!(
            env.svm.get_account(&env.market).unwrap(),
            market_before_split
        );
        assert_eq!(
            env.svm.get_account(&survivor).unwrap(),
            survivor_before_split
        );
        assert_eq!(
            env.svm.get_account(&successor).unwrap(),
            successor_before_split
        );
        assert_eq!(env.svm.get_account(&env.vault).unwrap(), vault_before_split);
        return;
    }
    env.trade(
        &survivor_owner,
        survivor,
        &successor_owner,
        successor,
        0,
        -(second_q as i128),
        PRICE,
    )
    .expect("second split transfer");
    assert_eq!(
        env.basis_q(survivor).unwrap().unsigned_abs(),
        raw_q - transfer_q
    );
    assert_eq!(env.basis_q(successor).unwrap().unsigned_abs(), transfer_q);
    let residual_capital_at_risk = env.portfolio(survivor).capital.get();

    let after_transfer = env.asset();
    assert_eq!(
        after_transfer.oi_eff_long_q,
        after_liquidation.oi_eff_long_q
    );
    let successor_value_before =
        env.portfolio(successor).capital.get() as i128 + env.pnl(successor);
    let liquidated_value_before =
        env.portfolio(liquidated).capital.get() as i128 + env.pnl(liquidated);

    env.set_slot(36);
    env.push_mark(0, 36, PRICE + PRICE_MOVE);
    for slot in 36..=75 {
        env.set_slot(slot);
        for _ in 0..2 {
            let _ = env.try_crank(successor, 0, slot);
            let _ = env.try_crank(liquidated, 0, slot);
        }
    }

    let winner_pnl = env.pnl(successor);
    let loser_pnl = env.pnl(liquidated);
    let successor_value_after = env.portfolio(successor).capital.get() as i128 + winner_pnl;
    let liquidated_value_after = env.portfolio(liquidated).capital.get() as i128 + loser_pnl;
    let winner_gain = successor_value_after - successor_value_before;
    let loser_loss = liquidated_value_before - liquidated_value_after;

    env.trade(
        &successor_owner,
        successor,
        &liquidated_owner,
        liquidated,
        0,
        -(first_q as i128),
        PRICE + PRICE_MOVE,
    )
    .expect("first post-settlement split transfer");
    env.trade(
        &successor_owner,
        successor,
        &liquidated_owner,
        liquidated,
        0,
        -(second_q as i128),
        PRICE + PRICE_MOVE,
    )
    .expect("second post-settlement split transfer");
    assert!(env.basis_q(successor).is_none());
    assert!(env.basis_q(liquidated).is_none());
    let source_claim = env.source_claim(1);
    let unsupported_claim = u128::try_from(winner_gain - loser_loss).unwrap();
    let converted = u128::try_from(winner_pnl).unwrap();
    env.convert(&successor_owner, successor, converted)
        .expect("convert the unsupported released claim");
    env.withdraw_portfolio(&successor_owner, successor, successor_collateral, converted)
        .expect("withdraw the unsupported claim as real collateral");

    let successor_remainder = env.portfolio(successor).capital.get();
    env.withdraw_portfolio(
        &successor_owner,
        successor,
        successor_collateral,
        successor_remainder,
    )
    .expect("recover the successor's remaining deposit");
    let liquidated_remainder = env.portfolio(liquidated).capital.get();
    env.withdraw_portfolio(
        &liquidated_owner,
        liquidated,
        liquidated_collateral,
        liquidated_remainder,
    )
    .expect("recover the losing counterparty's remaining deposit");

    let attacker_deposits =
        u128::from(SURVIVOR_DEPOSIT + LIQUIDATED_DEPOSIT + LIQUIDATED_TOP_UP + SUCCESSOR_DEPOSIT);
    let attacker_recovered = u128::from(token_amount(&env.svm, successor_collateral))
        + u128::from(token_amount(&env.svm, liquidated_collateral));
    let attacker_profit = attacker_recovered
        .checked_sub(attacker_deposits)
        .expect("split-transfer coalition extracts more collateral than it deposits");
    assert_eq!(setup_fee_cost, 16_000);
    assert_eq!(residual_capital_at_risk, u128::from(SURVIVOR_DEPOSIT));
    assert_eq!(winner_gain, 520_000);
    assert_eq!(loser_loss, 280_040);
    assert_eq!(unsupported_claim, 239_960);
    assert_eq!(attacker_profit, 173_960);
    assert_eq!(
        attacker_profit + setup_fee_cost + residual_capital_at_risk,
        unsupported_claim
    );

    let full_provider_return =
        env.withdraw_backing(&provider, provider_collateral, 1, PROVIDER_BACKING as u128);
    assert!(
        full_provider_return.is_err(),
        "independent provider must expose the unsupported claim as a real backing shortfall"
    );
    let provider_remainder = u128::from(PROVIDER_BACKING) - unsupported_claim;
    env.withdraw_backing(&provider, provider_collateral, 1, provider_remainder)
        .expect("provider recovers only backing not consumed by the excess claim");
    assert_eq!(
        u128::from(token_amount(&env.svm, provider_collateral)),
        provider_remainder
    );
    assert_eq!(
        u128::from(token_amount(&env.svm, env.vault)),
        setup_fee_cost + residual_capital_at_risk
    );

    panic!(
        "split post-ADL basis reissued a {unsupported_claim}-atom claim, returned {attacker_recovered} of {attacker_deposits} attacker deposits for {attacker_profit} net profit even after abandoning {residual_capital_at_risk} atoms, and left the independent provider {unsupported_claim} atoms short (effective OI {} -> {}, raw basis {raw_q}, source claim {source_claim}, loser PnL {loser_pnl})",
        after_liquidation.oi_eff_long_q, after_transfer.oi_eff_long_q,
    );
}
