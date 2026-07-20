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
        const ASSETS: usize = 4;

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
        .expect("permissionless crank");
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
    ) {
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
        .expect("withdraw converted capital");
    }

    fn refresh_flat_certificate(
        &mut self,
        owner: &Keypair,
        portfolio: Pubkey,
        counterparty_owner: &Keypair,
        counterparty: Pubkey,
        price: u64,
    ) {
        for size_q in [
            percolator::POS_SCALE as i128,
            -(percolator::POS_SCALE as i128),
        ] {
            self.trade(
                owner,
                portfolio,
                counterparty_owner,
                counterparty,
                0,
                size_q,
                price,
            )
            .expect("bounded certificate refresh trade");
        }
    }

    fn pnl(&self, portfolio: Pubkey) -> i128 {
        percolator_prog::state::read_portfolio(&self.svm.get_account(&portfolio).unwrap().data)
            .unwrap()
            .pnl
            .get()
    }

    fn source_claim(&self, domain: usize) -> u128 {
        percolator_prog::state::read_market(&self.svm.get_account(&self.market).unwrap().data)
            .unwrap()
            .1
            .source_credit[domain]
            .positive_claim_bound_num
    }
}

// One provider may back only one domain. Aggregate conversion cannot consume that backing while
// retiring another domain's claim, then consume the same provider a second time.
#[test]
fn source_conversion_cannot_charge_one_backing_provider_twice() {
    const INITIAL_PRICE: u64 = 100;
    const MOVED_PRICE: u64 = 105;
    const SIZE_Q: i128 = 20 * percolator::POS_SCALE as i128;
    const CLAIM: u128 = 100;
    const LOWER_DOMAIN: usize = 1;
    const FUNDED_DOMAIN: usize = 3;

    let mut env = IsolationEnv::new();
    let provider = Keypair::new();
    env.rotate_backing_authority(1, &provider);
    let provider_collateral = env.top_up_backing(&provider, FUNDED_DOMAIN as u16, 200, 10);

    let winner = Keypair::new();
    let stale_loser = Keypair::new();
    let replacement = Keypair::new();
    let (winner_portfolio, winner_collateral) = env.create_portfolio(&winner, 1_000);
    let (stale_loser_portfolio, _) = env.create_portfolio(&stale_loser, 1_000);
    let (replacement_portfolio, _) = env.create_portfolio(&replacement, 1_000);

    for asset_index in [0, 1] {
        env.trade(
            &winner,
            winner_portfolio,
            &stale_loser,
            stale_loser_portfolio,
            asset_index,
            SIZE_Q,
            INITIAL_PRICE,
        )
        .expect("open two independent winner legs");
    }

    env.set_slot(2);
    for asset_index in [0, 1] {
        env.push_mark(asset_index, 2, MOVED_PRICE);
        env.crank(winner_portfolio, asset_index, 2);
    }
    assert_eq!(env.pnl(winner_portfolio), (2 * CLAIM) as i128);
    assert_eq!(
        env.source_claim(LOWER_DOMAIN),
        CLAIM * percolator::BOUND_SCALE
    );
    assert_eq!(
        env.source_claim(FUNDED_DOMAIN),
        CLAIM * percolator::BOUND_SCALE
    );

    for asset_index in [0, 1] {
        env.trade(
            &winner,
            winner_portfolio,
            &replacement,
            replacement_portfolio,
            asset_index,
            -SIZE_Q,
            MOVED_PRICE,
        )
        .expect("flatten winner without refreshing original loser");
    }

    env.convert(&winner, winner_portfolio, CLAIM)
        .expect("first funded conversion");
    let lower_after_first = env.source_claim(LOWER_DOMAIN);
    let funded_after_first = env.source_claim(FUNDED_DOMAIN);
    env.refresh_flat_certificate(
        &winner,
        winner_portfolio,
        &replacement,
        replacement_portfolio,
        MOVED_PRICE,
    );
    let second_conversion = env.convert(&winner, winner_portfolio, CLAIM);

    let extracted = if second_conversion.is_ok() {
        env.refresh_flat_certificate(
            &winner,
            winner_portfolio,
            &replacement,
            replacement_portfolio,
            MOVED_PRICE,
        );
        2 * CLAIM
    } else {
        CLAIM
    };
    env.withdraw_portfolio(&winner, winner_portfolio, winner_collateral, extracted);
    assert_eq!(token_amount(&env.svm, winner_collateral), extracted as u64);

    for asset_index in [0, 1] {
        env.crank(stale_loser_portfolio, asset_index, 2);
        env.trade(
            &stale_loser,
            stale_loser_portfolio,
            &replacement,
            replacement_portfolio,
            asset_index,
            SIZE_Q,
            MOVED_PRICE,
        )
        .expect("settle and flatten the original losing side");
    }

    if second_conversion.is_ok() {
        assert!(
            env.withdraw_backing(&provider, provider_collateral, FUNDED_DOMAIN as u16, 200,)
                .is_err(),
            "double-charged backing cannot be fully reclaimed"
        );
        env.withdraw_backing(&provider, provider_collateral, FUNDED_DOMAIN as u16, 100)
            .expect("provider recovers only the locally replenished half");
        assert_eq!(token_amount(&env.svm, provider_collateral), 100);
        panic!(
            "domain mismatch extracted {extracted} atoms and left the independent provider with 100 of 200; first burn left claims lower={lower_after_first} funded={funded_after_first}"
        );
    }

    env.withdraw_backing(&provider, provider_collateral, FUNDED_DOMAIN as u16, 200)
        .expect("domain-local claim burn preserves all provider principal");
    assert_eq!(token_amount(&env.svm, provider_collateral), 200);
    assert_eq!(lower_after_first, CLAIM * percolator::BOUND_SCALE);
    assert_eq!(funded_after_first, 0);
}
