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

struct ExpiryEnv {
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

impl ExpiryEnv {
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
                    maintenance_fee_per_slot: 0,
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

    fn close_resolved_steps(&mut self, owner: &Keypair, portfolio: Pubkey, steps: usize) -> u128 {
        let mut payout = 0u128;
        for _ in 0..steps {
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
        payout
    }

    fn claim_topup(&mut self, owner: &Keypair, portfolio: Pubkey) -> (u128, Result<(), String>) {
        let destination = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            destination,
            self.collateral,
            owner.pubkey(),
            0,
        );
        let result = self.send(
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
                PIx::ClaimResolvedPayoutTopup,
            ),
            &[],
        );
        (u128::from(token_amount(&self.svm, destination)), result)
    }

    fn close_portfolio(&mut self, owner: &Keypair, portfolio: Pubkey) {
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
    }

    fn try_withdraw_terminal_insurance_without_ledger(
        &mut self,
        authority: &Keypair,
        amount: u128,
    ) -> (Pubkey, Result<(), String>) {
        let destination = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            destination,
            self.collateral,
            authority.pubkey(),
            0,
        );
        let result = self.send(
            pix(
                vec![
                    AccountMeta::new(authority.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new(self.vault, false),
                    AccountMeta::new_readonly(self.vault_authority, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::WithdrawInsurance { amount },
            ),
            &[authority],
        );
        (destination, result)
    }

    fn try_close_slab(&mut self) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let destination = Pubkey::new_unique();
        set_token(
            &mut self.svm,
            destination,
            self.collateral,
            admin.pubkey(),
            0,
        );
        self.send(
            pix(
                vec![
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(self.market, false),
                    AccountMeta::new(self.vault, false),
                    AccountMeta::new_readonly(self.vault_authority, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new_readonly(spl_token::ID, false),
                ],
                PIx::CloseSlab,
            ),
            &[&admin],
        )
    }
}

// A Fresh source bucket is senior to junior PnL until expiry. If it expires after the resolved
// payout snapshot, the released atoms must raise the common receipt rate instead of becoming an
// ownerless terminal vault residue. The close ordering below is permissionless.
#[test]
fn post_snapshot_backing_expiry_preserves_all_winner_entitlements() {
    const CAPITAL: u64 = 1_000;
    const OPEN_PRICE: u64 = 100;
    const WIN_PRICE: u64 = 140;

    let mut env = ExpiryEnv::new();
    let victim_owner = Keypair::new();
    let victim_loser_owner = Keypair::new();
    let ordered_first_owner = Keypair::new();
    let ordered_first_loser_owner = Keypair::new();
    let keeper_owner = Keypair::new();
    let (victim, _) = env.create_portfolio(&victim_owner, CAPITAL);
    let (victim_loser, _) = env.create_portfolio(&victim_loser_owner, CAPITAL);
    let (ordered_first, _) = env.create_portfolio(&ordered_first_owner, CAPITAL);
    let (ordered_first_loser, _) = env.create_portfolio(&ordered_first_loser_owner, CAPITAL);
    let (keeper, _) = env.create_portfolio(&keeper_owner, 0);

    env.trade(
        &victim_owner,
        victim,
        &victim_loser_owner,
        victim_loser,
        0,
        percolator::POS_SCALE as i128,
        OPEN_PRICE,
    )
    .expect("open independent asset-0 pair");
    env.trade(
        &ordered_first_owner,
        ordered_first,
        &ordered_first_loser_owner,
        ordered_first_loser,
        1,
        percolator::POS_SCALE as i128,
        OPEN_PRICE,
    )
    .expect("open independent asset-1 pair");

    env.set_slot(2);
    env.push_mark(0, 2, WIN_PRICE);
    env.push_mark(1, 2, WIN_PRICE);
    env.try_crank(
        keeper,
        2,
        vec![
            percolator_prog::ix::CrankObservationHint {
                asset_index: 0,
                oracle_accounts: 0,
            },
            percolator_prog::ix::CrankObservationHint {
                asset_index: 1,
                oracle_accounts: 0,
            },
        ],
    )
    .expect("public keeper publishes both authenticated marks");
    for portfolio in [victim_loser, victim] {
        env.crank(portfolio, 0, 2);
    }
    for portfolio in [ordered_first_loser, ordered_first] {
        env.crank(portfolio, 1, 2);
    }

    env.trade(
        &victim_owner,
        victim,
        &victim_loser_owner,
        victim_loser,
        0,
        -(percolator::POS_SCALE as i128),
        WIN_PRICE,
    )
    .expect("flatten victim pair while its source bucket remains Fresh");
    env.trade(
        &ordered_first_owner,
        ordered_first,
        &ordered_first_loser_owner,
        ordered_first_loser,
        1,
        -(percolator::POS_SCALE as i128),
        WIN_PRICE,
    )
    .expect("flatten ordered-first pair while its source bucket remains Fresh");
    for portfolio in [victim, victim_loser, ordered_first, ordered_first_loser] {
        let account = env.portfolio(portfolio);
        assert!(percolator::active_bitmap_is_empty(
            account.active_bitmap.map(percolator::V16PodU64::get)
        ));
    }
    let pre = env.group();
    assert_eq!(pre.assets[0].effective_price, WIN_PRICE);
    assert_eq!(pre.assets[1].effective_price, WIN_PRICE);
    for domain in [1usize, 3] {
        assert_eq!(
            pre.source_backing_buckets[domain].status,
            percolator::BackingBucketStatusV16::Fresh
        );
    }
    let lapse_slot = pre.source_backing_buckets[3]
        .expiry_slot
        .max(pre.source_backing_buckets[1].expiry_slot)
        .checked_add(1)
        .unwrap();
    env.set_slot(lapse_slot);

    env.resolve();
    let victim_loser_out = env.close_resolved_steps(&victim_loser_owner, victim_loser, 8);
    let ordered_first_loser_out =
        env.close_resolved_steps(&ordered_first_loser_owner, ordered_first_loser, 8);
    let mut ordered_first_out = env.close_resolved_steps(&ordered_first_owner, ordered_first, 8);
    let after_snapshot = env.group();
    assert!(after_snapshot.payout_snapshot_captured);
    assert_eq!(
        after_snapshot.source_backing_buckets[1].status,
        percolator::BackingBucketStatusV16::Fresh,
        "the first winner cannot expire the independent winner's source bucket"
    );
    assert_eq!(
        after_snapshot.source_backing_buckets[3].status,
        percolator::BackingBucketStatusV16::Expired,
        "the first resolved winner expires only its own lapsed source bucket"
    );

    let victim_out = env.close_resolved_steps(&victim_owner, victim, 8);
    ordered_first_out += env.close_resolved_steps(&ordered_first_owner, ordered_first, 8);
    let (ordered_first_topup, ordered_first_topup_result) =
        env.claim_topup(&ordered_first_owner, ordered_first);
    let (victim_topup, victim_topup_result) = env.claim_topup(&victim_owner, victim);
    ordered_first_topup_result.expect("receipt top-up remains callable");
    victim_topup_result.expect("receipt top-up remains callable");
    ordered_first_out += ordered_first_topup;
    let victim_out = victim_out + victim_topup;
    let done = env.group();

    if done.vault != 0 {
        assert_eq!(victim_loser_out, 960);
        assert_eq!(ordered_first_loser_out, 960);
        assert_eq!(ordered_first_out, 1_020);
        assert_eq!(victim_out, 1_020);
        assert_eq!(done.vault, 40);
        assert_eq!(done.payout_snapshot, 40);
        assert_eq!(
            done.resolved_payout_ledger.current_payout_rate_num,
            40 * percolator::BOUND_SCALE
        );
        assert_eq!(
            done.resolved_payout_ledger.current_payout_rate_den,
            80 * percolator::BOUND_SCALE
        );
        assert_eq!(ordered_first_topup, 0);
        assert_eq!(victim_topup, 0);
    }

    for (owner, portfolio) in [
        (&victim_owner, victim),
        (&victim_loser_owner, victim_loser),
        (&ordered_first_owner, ordered_first),
        (&ordered_first_loser_owner, ordered_first_loser),
        (&keeper_owner, keeper),
    ] {
        env.close_portfolio(owner, portfolio);
    }
    let terminal = env.group();
    assert_eq!(terminal.materialized_portfolio_count, 0);
    assert_eq!(terminal.c_tot, 0);
    assert_eq!(terminal.insurance, 0);
    assert_eq!(
        u128::from(token_amount(&env.svm, env.vault)),
        terminal.vault
    );

    let admin = env.admin.insecure_clone();
    let (insurance_destination, insurance_result) =
        env.try_withdraw_terminal_insurance_without_ledger(&admin, terminal.vault.max(1));
    assert!(insurance_result.is_err());
    assert_eq!(token_amount(&env.svm, insurance_destination), 0);

    let market_before_close = env.svm.get_account(&env.market).unwrap();
    let vault_before_close = env.svm.get_account(&env.vault).unwrap();
    let close_slab_result = env.try_close_slab();
    if terminal.vault != 0 {
        assert!(close_slab_result.is_err());
        assert_eq!(
            env.svm.get_account(&env.market).unwrap(),
            market_before_close
        );
        assert_eq!(env.svm.get_account(&env.vault).unwrap(), vault_before_close);
    } else {
        close_slab_result.expect("zero-residual market closes completely");
    }

    let total_payout = victim_loser_out + ordered_first_loser_out + ordered_first_out + victim_out;
    assert_eq!(
        terminal.vault,
        0,
        "post-snapshot expiry stranded {} atoms after every bounded continuation; \
         payouts=({victim_loser_out},{ordered_first_loser_out},{ordered_first_out},{victim_out}), \
         total={total_payout}, snapshot={}, rate={}/{}",
        terminal.vault,
        terminal.payout_snapshot,
        terminal.resolved_payout_ledger.current_payout_rate_num,
        terminal.resolved_payout_ledger.current_payout_rate_den,
    );
    assert_eq!(total_payout, 4 * u128::from(CAPITAL));
    assert_eq!(ordered_first_out, u128::from(CAPITAL) + 40);
    assert_eq!(victim_out, u128::from(CAPITAL) + 40);
}
