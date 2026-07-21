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
    oracle: Keypair,
    provider: Keypair,
    market: Pubkey,
    collateral: Pubkey,
    vault: Pubkey,
    vault_authority: Pubkey,
    portfolio_len: usize,
}

impl Env {
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
        let provider = Keypair::new();
        for signer in [&payer, &admin, &oracle, &provider] {
            svm.airdrop(&signer.pubkey(), 100_000_000_000).unwrap();
        }
        svm.set_sysvar(&Clock {
            slot: 100,
            unix_timestamp: 100,
            ..Clock::default()
        });

        let collateral = create_mint(&mut svm, &payer, admin.pubkey());
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
            provider,
            market,
            collateral,
            vault,
            vault_authority,
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(ASSETS)
                .unwrap(),
        };
        let provider = env.provider.insecure_clone();
        env.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
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
            &[&provider],
        )
        .expect("initialize a normal live market");
        for asset_index in [0, 1] {
            env.send(
                pix(
                    vec![
                        AccountMeta::new(provider.pubkey(), true),
                        AccountMeta::new(market, false),
                    ],
                    PIx::ConfigureAuthMark {
                        asset_index,
                        now_slot: 100,
                        initial_mark_e6: PRICE,
                    },
                ),
                &[&provider],
            )
            .expect("configure the authenticated mark");
        }

        let admin = env.admin.insecure_clone();
        env.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::UpdateAssetAuthority {
                    asset_index: 1,
                    kind: percolator_prog::processor::ASSET_AUTH_ADMIN,
                    new_pubkey: admin.pubkey().to_bytes(),
                },
            ),
            &[&provider, &admin],
        )
        .expect("remove the provider's asset-one administration");
        env.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(admin.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::UpdateAuthority {
                    new_pubkey: admin.pubkey().to_bytes(),
                },
            ),
            &[&provider, &admin],
        )
        .expect("remove the provider's global market authority");
        let oracle = env.oracle.insecure_clone();
        for (asset_index, kind, replacement) in [
            (
                0,
                percolator_prog::processor::ASSET_AUTH_INSURANCE,
                provider.pubkey(),
            ),
            (
                0,
                percolator_prog::processor::ASSET_AUTH_INSURANCE_OPERATOR,
                provider.pubkey(),
            ),
            (
                0,
                percolator_prog::processor::ASSET_AUTH_ORACLE,
                oracle.pubkey(),
            ),
            (
                1,
                percolator_prog::processor::ASSET_AUTH_INSURANCE,
                admin.pubkey(),
            ),
            (
                1,
                percolator_prog::processor::ASSET_AUTH_INSURANCE_OPERATOR,
                admin.pubkey(),
            ),
            (
                1,
                percolator_prog::processor::ASSET_AUTH_BACKING_BUCKET,
                admin.pubkey(),
            ),
            (
                1,
                percolator_prog::processor::ASSET_AUTH_ORACLE,
                oracle.pubkey(),
            ),
        ] {
            let replacement_signers = if replacement == provider.pubkey() {
                vec![&admin, &provider]
            } else if replacement == oracle.pubkey() {
                vec![&admin, &oracle]
            } else {
                vec![&admin]
            };
            env.send(
                pix(
                    vec![
                        AccountMeta::new(admin.pubkey(), true),
                        AccountMeta::new(replacement, true),
                        AccountMeta::new(market, false),
                    ],
                    PIx::UpdateAssetAuthority {
                        asset_index,
                        kind,
                        new_pubkey: replacement.to_bytes(),
                    },
                ),
                &replacement_signers,
            )
            .expect("separate the insurance and oracle roles");
        }

        let market_data = env.svm.get_account(&market).unwrap().data;
        let (config, _) = percolator_prog::state::read_market(&market_data).unwrap();
        let profile = percolator_prog::state::read_asset_oracle_profile(&market_data, 0).unwrap();
        let asset_one = percolator_prog::state::read_asset_oracle_profile(&market_data, 1).unwrap();
        assert_eq!(config.marketauth, admin.pubkey().to_bytes());
        assert_eq!(profile.asset_admin, admin.pubkey().to_bytes());
        assert_eq!(profile.insurance_authority, provider.pubkey().to_bytes(),);
        assert_eq!(profile.insurance_operator, provider.pubkey().to_bytes(),);
        assert_eq!(profile.backing_bucket_authority, admin.pubkey().to_bytes(),);
        assert_eq!(profile.oracle_authority, env.oracle.pubkey().to_bytes());
        assert_eq!(asset_one.asset_admin, admin.pubkey().to_bytes());
        assert_eq!(asset_one.insurance_authority, admin.pubkey().to_bytes());
        assert_eq!(asset_one.insurance_operator, admin.pubkey().to_bytes());
        assert_eq!(
            asset_one.backing_bucket_authority,
            admin.pubkey().to_bytes(),
        );
        assert_eq!(asset_one.oracle_authority, env.oracle.pubkey().to_bytes());
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
        .expect("initialize portfolio");
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

    fn try_crank(
        &mut self,
        portfolio: Pubkey,
        asset_index: u16,
        slot: u64,
        observe: bool,
    ) -> Result<(), String> {
        let observations = if observe {
            vec![percolator_prog::ix::CrankObservationHint {
                asset_index,
                oracle_accounts: 0,
            }]
        } else {
            vec![]
        };
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
}

fn try_withdraw_asset_zero_insurance(
    env: &mut Env,
    provider: &Keypair,
    amount: u64,
) -> (Pubkey, Result<(), String>) {
    let destination = Pubkey::new_unique();
    set_token(
        &mut env.svm,
        destination,
        env.collateral,
        provider.pubkey(),
        0,
    );
    let result = env.send(
        pix(
            vec![
                AccountMeta::new(provider.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(destination, false),
                AccountMeta::new(env.vault, false),
                AccountMeta::new_readonly(env.vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::WithdrawInsuranceAsset {
                asset_index: 0,
                amount: u128::from(amount),
            },
        ),
        &[&provider],
    );
    (destination, result)
}

// Bankruptcy on one permissionlessly traded asset must not freeze principal belonging to an
// external insurer of a different, empty asset. Public liquidation work on the failed asset must
// not be required to mutate an unrelated asset's custody authority or force a global shutdown.
#[test]
fn asset_one_bankruptcy_cannot_freeze_empty_asset_zero_insurance() {
    const PRICE: u64 = 100;
    const INSURANCE: u64 = 1_000;
    const WITHDRAWAL: u64 = 100;

    let mut env = Env::new();
    let provider = env.provider.insecure_clone();
    let provider_source = Pubkey::new_unique();
    set_token(
        &mut env.svm,
        provider_source,
        env.collateral,
        provider.pubkey(),
        INSURANCE,
    );
    env.send(
        pix(
            vec![
                AccountMeta::new(provider.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(provider_source, false),
                AccountMeta::new(env.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            PIx::TopUpInsuranceDomain {
                domain: 0,
                amount: u128::from(INSURANCE),
            },
        ),
        &[&provider],
    )
    .expect("external provider funds only asset zero");

    let (healthy_destination, healthy_withdrawal) =
        try_withdraw_asset_zero_insurance(&mut env, &provider, WITHDRAWAL);
    healthy_withdrawal.expect("empty asset-zero insurance is live before the attack");
    assert_eq!(token_amount(&env.svm, healthy_destination), WITHDRAWAL);

    let long_owner = Keypair::new();
    let short_owner = Keypair::new();
    let long = env.create_portfolio(&long_owner, 1_000_000);
    let short = env.create_portfolio(&short_owner, 250);
    env.send(
        pix(
            vec![
                AccountMeta::new(long_owner.pubkey(), true),
                AccountMeta::new(short_owner.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(long, false),
                AccountMeta::new(short, false),
            ],
            PIx::TradeNoCpi {
                asset_index: 1,
                size_q: percolator::POS_SCALE as i128,
                exec_price: PRICE,
                fee_bps: 0,
            },
        ),
        &[&long_owner, &short_owner],
    )
    .expect("unprivileged traders open balanced asset-one exposure");

    let oracle = env.oracle.insecure_clone();
    for (slot, mark) in [(101, 200), (102, 400), (103, 800)] {
        env.set_slot(slot);
        env.send(
            pix(
                vec![
                    AccountMeta::new(oracle.pubkey(), true),
                    AccountMeta::new(env.market, false),
                ],
                PIx::PushAuthMark {
                    asset_index: 1,
                    now_slot: slot,
                    mark_e6: mark,
                },
            ),
            &[&oracle],
        )
        .expect("independent oracle publishes an ordinary adverse mark");
        for portfolio in [long, short] {
            let _ = env.try_crank(portfolio, 1, slot, true);
        }
    }
    for _ in 0..4 {
        let _ = env.try_crank(short, 1, 103, false);
    }

    if let Ok((_, group)) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
    {
        assert!(group.bankruptcy_hlock_active);
        assert!(!group.threshold_stress_active);
        assert!(!group.loss_stale_active);
        assert_eq!(group.assets[0].oi_eff_long_q, 0);
        assert_eq!(group.assets[0].oi_eff_short_q, 0);
        assert_eq!(group.assets[0].loss_weight_sum_long, 0);
        assert_eq!(group.assets[0].loss_weight_sum_short, 0);
        let short_state =
            percolator_prog::state::read_portfolio(&env.svm.get_account(&short).unwrap().data)
                .unwrap();
        assert_eq!(short_state.capital.get(), 0);
    }

    let (first_destination, first_withdrawal) =
        try_withdraw_asset_zero_insurance(&mut env, &provider, WITHDRAWAL);

    let mut public_crank_attempts = 0u64;
    let mut successful_public_cranks = 0u64;
    for slot in 104..=119 {
        env.set_slot(slot);
        for portfolio in [long, short] {
            public_crank_attempts += 1;
            if env.try_crank(portfolio, 1, slot, false).is_ok() {
                successful_public_cranks += 1;
            }
        }
    }
    let (second_destination, second_withdrawal) =
        try_withdraw_asset_zero_insurance(&mut env, &provider, WITHDRAWAL);

    let first_paid = token_amount(&env.svm, first_destination);
    let second_paid = token_amount(&env.svm, second_destination);
    assert!(
        first_withdrawal.is_ok() && second_withdrawal.is_ok(),
        "asset-one bankruptcy froze unrelated asset-zero insurance before and after {public_crank_attempts} bounded public crank attempts ({successful_public_cranks} succeeded); first={first_withdrawal:?}, second={second_withdrawal:?}, first_paid={first_paid}, second_paid={second_paid}",
    );
    assert_eq!(first_paid, WITHDRAWAL);
    assert_eq!(second_paid, WITHDRAWAL);
}
