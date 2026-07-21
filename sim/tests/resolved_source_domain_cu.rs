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

const ASSET_COUNT: u16 = 14;
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
    market: Pubkey,
    collateral: Pubkey,
    vault: Pubkey,
    vault_authority: Pubkey,
    portfolio_len: usize,
}

impl Env {
    fn new() -> Self {
        const OPEN_PRICE: u64 = 100;

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
        svm.set_sysvar(&Clock::default());

        let collateral = create_mint(&mut svm, &payer, admin.pubkey());
        let market = Pubkey::new_unique();
        svm.set_account(
            market,
            Account {
                lamports: 1_000_000_000,
                data: vec![
                    0;
                    percolator_prog::state::market_account_len_for_capacity(
                        ASSET_COUNT as usize
                    )
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
            market,
            collateral,
            vault,
            vault_authority,
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(
                ASSET_COUNT as usize,
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
                    max_portfolio_assets: ASSET_COUNT,
                    h_min: 0,
                    h_max: 10,
                    initial_price: OPEN_PRICE,
                    min_nonzero_mm_req: 1,
                    min_nonzero_im_req: 2,
                    maintenance_margin_bps: 1_000,
                    initial_margin_bps: 1_000,
                    max_trading_fee_bps: 10_000,
                    trade_fee_base_bps: 0,
                    liquidation_fee_bps: 0,
                    liquidation_fee_cap: 0,
                    min_liquidation_abs: 0,
                    max_price_move_bps_per_slot: 500,
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
        .expect("initialize the normal 14-asset market");

        for asset_index in 0..ASSET_COUNT {
            env.send(
                pix(
                    vec![
                        AccountMeta::new(admin.pubkey(), true),
                        AccountMeta::new(market, false),
                    ],
                    PIx::ConfigureAuthMark {
                        asset_index,
                        now_slot: 0,
                        initial_mark_e6: OPEN_PRICE,
                    },
                ),
                &[&admin],
            )
            .expect("configure each authenticated mark");
        }
        env
    }

    fn send(&mut self, instruction: Instruction, signers: &[&Keypair]) -> Result<u64, String> {
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
            .map(|meta| meta.compute_units_consumed)
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
        .expect("initialize a normal portfolio");

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

    fn trade(
        &mut self,
        asset_index: u16,
        winner_owner: &Keypair,
        winner: Pubkey,
        loser_owner: &Keypair,
        loser: Pubkey,
        size_q: i128,
        price: u64,
    ) {
        self.send(
            pix(
                vec![
                    AccountMeta::new(winner_owner.pubkey(), true),
                    AccountMeta::new(loser_owner.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(winner, false),
                    AccountMeta::new(loser, false),
                ],
                PIx::TradeNoCpi {
                    asset_index,
                    size_q,
                    exec_price: price,
                    fee_bps: 0,
                },
            ),
            &[winner_owner, loser_owner],
        )
        .expect("ordinary users trade");
    }

    fn push_mark(&mut self, asset_index: u16, slot: u64, mark: u64) {
        let admin = self.admin.insecure_clone();
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(self.market, false),
                ],
                PIx::PushAuthMark {
                    asset_index,
                    now_slot: slot,
                    mark_e6: mark,
                },
            ),
            &[&admin],
        )
        .expect("authenticated oracle publishes the normal mark");
    }

    fn crank(&mut self, portfolio: Pubkey, slot: u64, asset_index: u16) {
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
        .expect("permissionless crank makes public progress");
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
        .expect("configured governance resolves the market normally");
    }
}

fn portfolio(env: &Env, key: Pubkey) -> percolator::PortfolioAccountV16Account {
    percolator_prog::state::read_portfolio(&env.svm.get_account(&key).unwrap().data).unwrap()
}

fn source_claim_count(account: &percolator::PortfolioAccountV16Account) -> usize {
    account
        .source_domains
        .iter()
        .filter(|source| source.source_claim_bound_num.get() != 0)
        .count()
}

fn active_leg_count(account: &percolator::PortfolioAccountV16Account) -> usize {
    account
        .legs
        .iter()
        .filter_map(|leg| leg.try_to_runtime().ok())
        .filter(|leg| leg.active)
        .count()
}

// Public max-shape regression: ordinary trades can leave a flat resolved winner with both source
// domains for all 14 assets. Direct close and its resolved-crank alias must reduce that finite rank
// in bounded CU; they may not both exhaust the SVM ceiling and roll back forever.
#[test]
fn resolved_close_with_28_source_domains_must_make_bounded_progress() {
    const OPEN_PRICE: u64 = 100;
    const HIGH_PRICE: u64 = 200;

    let mut env = Env::new();
    let winner_owner = Keypair::new();
    let winner = env.create_portfolio(&winner_owner, 100_000);
    let keeper_owner = Keypair::new();
    let keeper = env.create_portfolio(&keeper_owner, 0);

    let mut losers = Vec::new();
    for asset_index in 0..ASSET_COUNT {
        let owner = Keypair::new();
        let account = env.create_portfolio(&owner, 1_000);
        env.trade(
            asset_index,
            &winner_owner,
            winner,
            &owner,
            account,
            percolator::POS_SCALE as i128,
            OPEN_PRICE,
        );
        losers.push((owner, account));
    }

    env.set_slot(20);
    for asset_index in 0..ASSET_COUNT {
        env.push_mark(asset_index, 20, HIGH_PRICE);
        env.crank(keeper, 20, asset_index);
    }
    for (asset_index, (_, account)) in losers.iter().enumerate() {
        env.crank(*account, 20, asset_index as u16);
    }
    env.crank(winner, 20, 0);
    assert_eq!(source_claim_count(&portfolio(&env, winner)), 14);

    for (asset_index, (owner, account)) in losers.iter().enumerate() {
        env.trade(
            asset_index as u16,
            &winner_owner,
            winner,
            owner,
            *account,
            -((2 * percolator::POS_SCALE) as i128),
            HIGH_PRICE,
        );
    }

    env.set_slot(40);
    for asset_index in 0..ASSET_COUNT {
        env.push_mark(asset_index, 40, OPEN_PRICE);
        env.crank(keeper, 40, asset_index);
    }
    for (asset_index, (_, account)) in losers.iter().enumerate() {
        env.crank(*account, 40, asset_index as u16);
    }
    env.crank(winner, 40, 0);
    assert_eq!(source_claim_count(&portfolio(&env, winner)), 28);

    for (asset_index, (owner, account)) in losers.iter().enumerate() {
        env.trade(
            asset_index as u16,
            &winner_owner,
            winner,
            owner,
            *account,
            percolator::POS_SCALE as i128,
            OPEN_PRICE,
        );
    }

    let live_winner = portfolio(&env, winner);
    assert_eq!(active_leg_count(&live_winner), 0);
    assert_eq!(live_winner.capital.get(), 100_000);
    assert!(live_winner.pnl.get() > 0);
    assert_eq!(source_claim_count(&live_winner), 28);

    let (_, live_group) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    for bucket in &live_group.source_backing_buckets[..28] {
        assert_eq!(bucket.status, percolator::BackingBucketStatusV16::Fresh);
        assert!(bucket.expiry_slot > 40);
    }

    env.resolve();
    let destination = Pubkey::new_unique();
    set_token(
        &mut env.svm,
        destination,
        env.collateral,
        winner_owner.pubkey(),
        0,
    );

    let rank_before = source_claim_count(&portfolio(&env, winner));
    let market_before_direct = env.svm.get_account(&env.market).unwrap();
    let winner_before_direct = env.svm.get_account(&winner).unwrap();
    let vault_before_direct = env.svm.get_account(&env.vault).unwrap();
    let destination_before_direct = env.svm.get_account(&destination).unwrap();
    let direct = env.send(
        pix(
            vec![
                AccountMeta::new_readonly(winner_owner.pubkey(), false),
                AccountMeta::new(env.market, false),
                AccountMeta::new(winner, false),
                AccountMeta::new(destination, false),
                AccountMeta::new(env.vault, false),
                AccountMeta::new_readonly(env.vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::CloseResolved {
                fee_rate_per_slot: 0,
            },
        ),
        &[],
    );
    let rank_after_direct = source_claim_count(&portfolio(&env, winner));
    if direct.is_err() {
        assert_eq!(
            env.svm.get_account(&env.market).unwrap(),
            market_before_direct
        );
        assert_eq!(env.svm.get_account(&winner).unwrap(), winner_before_direct);
        assert_eq!(
            env.svm.get_account(&env.vault).unwrap(),
            vault_before_direct
        );
        assert_eq!(
            env.svm.get_account(&destination).unwrap(),
            destination_before_direct
        );
    } else if let Ok(cu) = direct.as_ref() {
        assert!(*cu <= 1_300_000, "direct progress consumed {cu} CU");
        assert_eq!(
            rank_after_direct + 1,
            rank_before,
            "direct close must remove exactly one source claim"
        );
    }

    let market_before_crank = env.svm.get_account(&env.market).unwrap();
    let winner_before_crank = env.svm.get_account(&winner).unwrap();
    let vault_before_crank = env.svm.get_account(&env.vault).unwrap();
    let crank = env.send(
        pix(
            vec![
                AccountMeta::new_readonly(winner_owner.pubkey(), false),
                AccountMeta::new(env.market, false),
                AccountMeta::new(winner, false),
            ],
            PIx::PermissionlessCrank {
                now_slot: 40,
                observations: vec![],
            },
        ),
        &[],
    );
    if crank.is_err() {
        assert_eq!(
            env.svm.get_account(&env.market).unwrap(),
            market_before_crank
        );
        assert_eq!(env.svm.get_account(&winner).unwrap(), winner_before_crank);
        assert_eq!(env.svm.get_account(&env.vault).unwrap(), vault_before_crank);
    }

    let terminal = portfolio(&env, winner);
    let rank_after = source_claim_count(&terminal);
    if let Ok(cu) = crank.as_ref() {
        assert!(*cu <= 1_300_000, "resolved crank progress consumed {cu} CU");
        assert_eq!(
            rank_after + 1,
            rank_after_direct,
            "resolved crank must remove exactly one source claim"
        );
    }
    let destination_after = token_amount(&env.svm, destination);
    assert_eq!(
        env.svm.get_account(&env.vault).unwrap(),
        vault_before_direct
    );
    assert_eq!(
        env.svm.get_account(&destination).unwrap(),
        destination_before_direct
    );
    assert!(
        rank_after < rank_before || terminal.capital.get() == 0,
        "both bounded terminal routes rolled back: direct={direct:?}, crank={crank:?}, rank_before={rank_before}, rank_after_direct={rank_after_direct}, rank_after={rank_after}, capital={}, pnl={}, destination_after={destination_after}",
        terminal.capital.get(),
        terminal.pnl.get(),
    );
}
