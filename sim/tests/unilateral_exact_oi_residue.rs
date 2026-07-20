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

struct ResidueEnv {
    svm: LiteSVM,
    payer: Keypair,
    market: Pubkey,
    collateral: Pubkey,
    vault: Pubkey,
    vault_authority: Pubkey,
    portfolio_len: usize,
}

impl ResidueEnv {
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
        svm.airdrop(&payer.pubkey(), 100_000_000_000_000).unwrap();
        svm.airdrop(&admin.pubkey(), 1_000_000_000).unwrap();
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
            market,
            collateral,
            vault,
            vault_authority,
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(1)
                .unwrap(),
        };
        env.send(
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
                    initial_price: PRICE,
                    min_nonzero_mm_req: 599,
                    min_nonzero_im_req: 600,
                    maintenance_margin_bps: 500,
                    initial_margin_bps: 500,
                    max_trading_fee_bps: 10_000,
                    trade_fee_base_bps: 0,
                    liquidation_fee_bps: 0,
                    liquidation_fee_cap: 0,
                    min_liquidation_abs: 0,
                    max_price_move_bps_per_slot: 24,
                    max_accrual_dt_slots: 20,
                    max_abs_funding_e9_per_slot: 0,
                    min_funding_lifetime_slots: 10_000_000,
                    max_account_b_settlement_chunks: 1,
                    max_bankrupt_close_chunks: 1,
                    max_bankrupt_close_lifetime_slots: 100,
                    public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
                    maintenance_fee_per_slot: 27,
                },
            ),
            &[&admin],
        )
        .expect("initialize live liquidation market");
        env.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index: 0,
                    now_slot: 1,
                    initial_mark_e6: PRICE,
                },
            ),
            &[&admin],
        )
        .expect("configure unchanged authenticated mark");
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
        size_q: i128,
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
                    asset_index: 0,
                    size_q,
                    exec_price: 1,
                    fee_bps: 0,
                },
            ),
            &[owner_a, owner_b],
        )
    }

    fn crank(&mut self, portfolio: Pubkey, slot: u64) -> Result<(), String> {
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
                        asset_index: 0,
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

    fn rebalance(
        &mut self,
        owner: &Keypair,
        portfolio: Pubkey,
        reduce_q: u128,
    ) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(self.market, false),
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

    fn forfeit(&mut self, owner: &Keypair, portfolio: Pubkey) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new(owner.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(portfolio, false),
                ],
                PIx::ForfeitRecoveryLeg {
                    asset_index: 0,
                    b_delta_budget: percolator::MAX_VAULT_TVL,
                },
            ),
            &[owner],
        )
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

    fn close_portfolio(&mut self, owner: &Keypair, portfolio: Pubkey) -> Result<(), String> {
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
    }

    fn asset(&self) -> percolator::AssetStateV16 {
        let account = self.svm.get_account(&self.market).unwrap();
        percolator_prog::state::read_market(&account.data)
            .unwrap()
            .1
            .assets[0]
    }

    fn portfolio(&self, portfolio: Pubkey) -> percolator::PortfolioAccountV16Account {
        percolator_prog::state::read_portfolio(&self.svm.get_account(&portfolio).unwrap().data)
            .unwrap()
    }

    fn basis_q(&self, portfolio: Pubkey) -> Option<i128> {
        self.portfolio(portfolio)
            .legs
            .iter()
            .filter_map(|leg| leg.try_to_runtime().ok())
            .find(|leg| leg.active && leg.asset_index == 0)
            .map(|leg| leg.basis_pos_q)
    }

    fn capital(&self, portfolio: Pubkey) -> u128 {
        self.portfolio(portfolio).capital.get()
    }

    fn portfolio_is_closed(&self, portfolio: Pubkey) -> bool {
        self.svm
            .get_account(&portfolio)
            .is_none_or(|account| account.lamports == 0 && account.data.is_empty())
    }
}

// Once effective OI reaches zero, every stored survivor must enter the bounded reset path. A
// successful unilateral reduction cannot leave a Normal-mode leg that no public instruction owns.
#[test]
fn exact_effective_oi_rebalance_cannot_strand_adl_survivor() {
    const OPEN_Q: i128 = 13_000 * percolator::POS_SCALE as i128;

    let mut env = ResidueEnv::new();
    let survivor_owner = Keypair::new();
    let bankrupt_owner = Keypair::new();
    let (survivor, survivor_destination) = env.create_portfolio(&survivor_owner, 1_000);
    let (bankrupt, _) = env.create_portfolio(&bankrupt_owner, 1_189);

    env.set_slot(8);
    env.trade(&survivor_owner, survivor, &bankrupt_owner, bankrupt, OPEN_Q)
        .expect("open balanced interest");
    env.set_slot(27);
    env.crank(survivor, 27)
        .expect("refresh the future survivor");
    env.set_slot(35);
    env.sync_maintenance_fee(bankrupt, 35);
    env.crank(bankrupt, 35)
        .expect("certify the under-margined side");
    env.crank(bankrupt, 35)
        .expect("partially liquidate into ADL");

    let before = env.asset();
    let effective_q = before.oi_eff_long_q;
    let survivor_basis = env
        .basis_q(survivor)
        .expect("survivor retains an active leg")
        .unsigned_abs();
    assert_eq!(effective_q, before.oi_eff_short_q);
    assert!(effective_q > 0 && survivor_basis > effective_q);

    env.rebalance(&survivor_owner, survivor, effective_q)
        .expect("owner reduces exactly the remaining effective OI");
    let after_reduce = env.asset();
    let residual_q = env
        .basis_q(survivor)
        .expect("stored basis remains after exact effective-OI reduction")
        .unsigned_abs();
    let trapped_capital = env.capital(survivor);
    assert_eq!(
        (after_reduce.oi_eff_long_q, after_reduce.oi_eff_short_q),
        (0, 0)
    );
    assert!(residual_q > 0 && trapped_capital > 0);

    // First remove the opposite account's legitimate reset obligation. This prevents its state
    // from being mistaken for the survivor's liveness failure.
    for slot in 36..=43 {
        env.set_slot(slot);
        let _ = env.crank(bankrupt, slot);
        if env.basis_q(bankrupt).is_none() {
            break;
        }
    }
    if env.basis_q(bankrupt).is_some() {
        let _ = env.forfeit(&bankrupt_owner, bankrupt);
    }
    for slot in 44..=51 {
        env.set_slot(slot);
        let _ = env.crank(bankrupt, slot);
        if env.basis_q(bankrupt).is_none() {
            break;
        }
    }
    assert!(
        env.basis_q(bankrupt).is_none(),
        "opposite-side cleanup must finish before survivor liveness is classified"
    );

    let willing_owner = Keypair::new();
    let (willing, _) = env.create_portfolio(&willing_owner, 1_000);
    let market_before_trade = env.svm.get_account(&env.market).unwrap();
    let survivor_before_trade = env.svm.get_account(&survivor).unwrap();
    let willing_before_trade = env.svm.get_account(&willing).unwrap();
    let willing_close = env.trade(
        &willing_owner,
        willing,
        &survivor_owner,
        survivor,
        residual_q as i128,
    );
    assert!(
        willing_close.is_err(),
        "fresh funded counterparty escape control"
    );
    assert_eq!(
        env.svm.get_account(&env.market).unwrap(),
        market_before_trade
    );
    assert_eq!(
        env.svm.get_account(&survivor).unwrap(),
        survivor_before_trade
    );
    assert_eq!(env.svm.get_account(&willing).unwrap(), willing_before_trade);

    let direct_reduce = env.rebalance(&survivor_owner, survivor, residual_q);
    let forfeit = env.forfeit(&survivor_owner, survivor);
    let early_withdraw = env.withdraw(
        &survivor_owner,
        survivor,
        survivor_destination,
        trapped_capital,
    );
    let early_close = env.close_portfolio(&survivor_owner, survivor);

    if env.portfolio_is_closed(survivor) {
        early_close.expect("fixed path closes only after the owner can recover principal");
        assert_eq!(
            token_amount(&env.svm, survivor_destination),
            trapped_capital as u64,
            "an immediate escape must return all trapped principal"
        );
        return;
    }

    for slot in 52..=67 {
        env.set_slot(slot);
        let _ = env.crank(survivor, slot);
        if env.basis_q(survivor).is_none() {
            break;
        }
    }

    if env.basis_q(survivor).is_none() {
        let withdrawable = env.capital(survivor);
        env.withdraw(
            &survivor_owner,
            survivor,
            survivor_destination,
            withdrawable,
        )
        .expect("bounded reset restores owner withdrawal");
        assert_eq!(
            token_amount(&env.svm, survivor_destination),
            withdrawable as u64
        );
        return;
    }

    let terminal_asset = env.asset();
    assert!(direct_reduce.is_err());
    assert!(forfeit.is_err());
    assert!(early_withdraw.is_err());
    assert!(early_close.is_err());
    assert_eq!(
        (terminal_asset.oi_eff_long_q, terminal_asset.oi_eff_short_q),
        (0, 0)
    );
    assert_eq!(env.basis_q(survivor).unwrap().unsigned_abs(), residual_q);
    assert_eq!(env.capital(survivor), trapped_capital);
    panic!(
        "exact-OI residue stayed {:?} with basis {residual_q}, zero OI, and {trapped_capital} atoms trapped after fresh counterparty, direct reduction, forfeit, withdraw, close, and 16 fresh-slot cranks",
        terminal_asset.mode_long
    );
}
