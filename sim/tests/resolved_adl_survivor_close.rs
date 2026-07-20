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

struct ResolvedEnv {
    svm: LiteSVM,
    payer: Keypair,
    admin: Keypair,
    market: Pubkey,
    collateral: Pubkey,
    vault: Pubkey,
    vault_authority: Pubkey,
    portfolio_len: usize,
}

impl ResolvedEnv {
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
            admin,
            market,
            collateral,
            vault,
            vault_authority,
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(1)
                .unwrap(),
        };
        let admin_signer = env.admin.insecure_clone();
        env.send(
            pix(
                vec![
                    AccountMeta::new(env.admin.pubkey(), true),
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
            &[&admin_signer],
        )
        .expect("initialize live liquidation market");
        env.send(
            pix(
                vec![
                    AccountMeta::new(env.admin.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index: 0,
                    now_slot: 1,
                    initial_mark_e6: PRICE,
                },
            ),
            &[&admin_signer],
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

    fn resolve(&mut self) {
        let admin = self.admin.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(self.admin.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::ResolveMarket,
            ),
            &[&admin],
        )
        .expect("resolve the live market through its configured authority");
    }

    fn close_resolved(
        &mut self,
        owner: &Keypair,
        portfolio: Pubkey,
        destination: Pubkey,
    ) -> Result<(), String> {
        self.send(
            pix(
                vec![
                    AccountMeta::new_readonly(owner.pubkey(), true),
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
}

// Resolution must not turn an otherwise bounded ADL survivor into an account whose only terminal
// close path subtracts more raw basis than the market's A-scaled effective open interest.
#[test]
fn resolved_close_cannot_strand_adl_survivor_principal() {
    const OPEN_Q: i128 = 13_000 * percolator::POS_SCALE as i128;

    let mut env = ResolvedEnv::new();
    let survivor_owner = Keypair::new();
    let bankrupt_owner = Keypair::new();
    let (survivor, survivor_destination) = env.create_portfolio(&survivor_owner, 10_000);
    let (bankrupt, bankrupt_destination) = env.create_portfolio(&bankrupt_owner, 1_189);

    env.set_slot(8);
    env.trade(&survivor_owner, survivor, &bankrupt_owner, bankrupt, OPEN_Q)
        .expect("open balanced interest");
    env.set_slot(27);
    env.crank(survivor, 27)
        .expect("advance the market within the bounded accrual window");
    env.set_slot(35);
    env.sync_maintenance_fee(bankrupt, 35);
    env.crank(bankrupt, 35)
        .expect("certify the under-margined side");
    env.crank(bankrupt, 35)
        .expect("partially liquidate into ADL");

    let before_resolve = env.asset();
    let survivor_basis = env
        .basis_q(survivor)
        .expect("the opposite-side survivor retains its stored leg")
        .unsigned_abs();
    assert!(before_resolve.oi_eff_long_q > 0);
    assert_eq!(before_resolve.oi_eff_long_q, before_resolve.oi_eff_short_q);
    assert!(
        survivor_basis > before_resolve.oi_eff_long_q,
        "liquidation must create the raw-basis versus effective-OI mismatch"
    );

    env.resolve();

    // Give the losing side every bounded terminal step first. The survivor's failure must not be
    // an ordering dependency on a still-actionable counterparty.
    for _ in 0..8 {
        if env.basis_q(bankrupt).is_none() {
            break;
        }
        let _ = env.close_resolved(&bankrupt_owner, bankrupt, bankrupt_destination);
    }
    assert!(
        env.basis_q(bankrupt).is_none(),
        "the losing side must finish its legitimate terminal work first"
    );

    let trapped_capital = env.capital(survivor);
    let destination_before = token_amount(&env.svm, survivor_destination);
    assert!(trapped_capital > 0);

    for slot in 36..=51 {
        env.set_slot(slot);
        let market_before = env.svm.get_account(&env.market).unwrap();
        let survivor_before = env.svm.get_account(&survivor).unwrap();
        let vault_before = env.svm.get_account(&env.vault).unwrap();
        let close = env.close_resolved(&survivor_owner, survivor, survivor_destination);
        if close.is_ok() && env.basis_q(survivor).is_none() {
            assert!(
                token_amount(&env.svm, survivor_destination) > destination_before,
                "a successful fixed close must return the surviving principal"
            );
            env.close_portfolio(&survivor_owner, survivor)
                .expect("terminally settled portfolio remains owner-closeable");
            return;
        }
        assert!(close.is_err(), "a successful close must clear the only leg");
        assert_eq!(env.svm.get_account(&env.market).unwrap(), market_before);
        assert_eq!(env.svm.get_account(&survivor).unwrap(), survivor_before);
        assert_eq!(env.svm.get_account(&env.vault).unwrap(), vault_before);
    }

    let withdrawal = env.withdraw(
        &survivor_owner,
        survivor,
        survivor_destination,
        trapped_capital,
    );
    let portfolio_close = env.close_portfolio(&survivor_owner, survivor);
    let terminal_asset = env.asset();
    assert!(withdrawal.is_err());
    assert!(portfolio_close.is_err());
    assert_eq!(
        env.basis_q(survivor).unwrap().unsigned_abs(),
        survivor_basis
    );
    assert_eq!(env.capital(survivor), trapped_capital);
    assert_eq!(
        token_amount(&env.svm, survivor_destination),
        destination_before
    );
    panic!(
        "resolved ADL survivor retained basis {survivor_basis} above effective OI {} and trapped {trapped_capital} atoms after loser cleanup, 16 fully supplied CloseResolved attempts, withdrawal, and portfolio close",
        terminal_asset.oi_eff_long_q
    );
}
