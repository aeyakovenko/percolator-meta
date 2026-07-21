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
    portfolio_len: usize,
}

impl Env {
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
            oracle,
            provider,
            market,
            collateral,
            vault,
            portfolio_len: percolator_prog::state::portfolio_account_len_for_market_slots(1)
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
                    max_portfolio_assets: 1,
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
        env.send(
            pix(
                vec![
                    AccountMeta::new(provider.pubkey(), true),
                    AccountMeta::new(market, false),
                ],
                PIx::ConfigureAuthMark {
                    asset_index: 0,
                    now_slot: 100,
                    initial_mark_e6: PRICE,
                },
            ),
            &[&provider],
        )
        .expect("configure the authenticated mark");

        let admin = env.admin.insecure_clone();
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
        for (kind, replacement) in [
            (
                percolator_prog::processor::ASSET_AUTH_BACKING_BUCKET,
                provider.pubkey(),
            ),
            (
                percolator_prog::processor::ASSET_AUTH_ORACLE,
                oracle.pubkey(),
            ),
        ] {
            let replacement_signer = if replacement == provider.pubkey() {
                &provider
            } else {
                &oracle
            };
            env.send(
                pix(
                    vec![
                        AccountMeta::new(admin.pubkey(), true),
                        AccountMeta::new(replacement, true),
                        AccountMeta::new(market, false),
                    ],
                    PIx::UpdateAssetAuthority {
                        asset_index: 0,
                        kind,
                        new_pubkey: replacement.to_bytes(),
                    },
                ),
                &[&admin, replacement_signer],
            )
            .expect("separate the backing and oracle roles");
        }

        let market_data = env.svm.get_account(&market).unwrap().data;
        let (config, _) = percolator_prog::state::read_market(&market_data).unwrap();
        let profile = percolator_prog::state::read_asset_oracle_profile(&market_data, 0).unwrap();
        assert_eq!(config.marketauth, admin.pubkey().to_bytes());
        assert_eq!(profile.asset_admin, admin.pubkey().to_bytes());
        assert_eq!(profile.insurance_authority, admin.pubkey().to_bytes());
        assert_eq!(profile.insurance_operator, admin.pubkey().to_bytes());
        assert_eq!(
            profile.backing_bucket_authority,
            provider.pubkey().to_bytes(),
        );
        assert_eq!(profile.oracle_authority, env.oracle.pubkey().to_bytes());
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

    fn try_crank(&mut self, portfolio: Pubkey, slot: u64, observe: bool) -> Result<(), String> {
        let observations = if observe {
            vec![percolator_prog::ix::CrankObservationHint {
                asset_index: 0,
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

    fn crank(&mut self, portfolio: Pubkey, slot: u64, observe: bool) {
        self.try_crank(portfolio, slot, observe)
            .expect("permissionless crank makes progress");
    }
}

fn portfolio_state(env: &Env, portfolio: Pubkey) -> percolator::PortfolioAccountV16Account {
    percolator_prog::state::read_portfolio(&env.svm.get_account(&portfolio).unwrap().data).unwrap()
}

fn active_leg(
    account: &percolator::PortfolioAccountV16Account,
) -> Option<percolator::PortfolioLegV16> {
    account
        .legs
        .iter()
        .filter_map(|leg| leg.try_to_runtime().ok())
        .find(|leg| leg.active && leg.asset_index == 0)
}

fn try_reduce(env: &mut Env, owner: &Keypair, portfolio: Pubkey) -> Result<(), String> {
    env.send(
        pix(
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(env.market, false),
                AccountMeta::new(portfolio, false),
            ],
            PIx::RebalanceReduce {
                asset_index: 0,
                reduce_q: percolator::POS_SCALE,
            },
        ),
        &[owner],
    )
}

fn settle_social_loss(env: &mut Env, portfolio: Pubkey, slot: u64) {
    for _ in 0..8 {
        let account = portfolio_state(env, portfolio);
        let leg = active_leg(&account).expect("winner remains exposed before owner reduction");
        let (_, group) =
            percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
                .unwrap();
        if !leg.b_stale && leg.b_snap == group.assets[0].b_long_num {
            return;
        }
        env.try_crank(portfolio, slot, false)
            .expect("public crank settles the winner's social-loss index");
    }
    panic!("winner did not settle social loss in bounded public calls");
}

// Two ordinary exits may each fold a half-atom B remainder into side dust. The second owner must
// rebook the carried whole atom over remaining loss weight and flatten; it may not return
// RecoveryRequired forever while the live market and the owner's principal remain locked.
#[test]
fn social_loss_dust_carry_must_not_block_the_second_owner_exit() {
    let mut env = Env::new();

    let l1o = Keypair::new();
    let l2o = Keypair::new();
    let l3o = Keypair::new();
    let l4o = Keypair::new();
    let s1o = Keypair::new();
    let s2o = Keypair::new();
    let s3o = Keypair::new();
    let s4o = Keypair::new();

    let l1 = env.create_portfolio(&l1o, 1_000);
    let l2 = env.create_portfolio(&l2o, 1_000);
    let l3 = env.create_portfolio(&l3o, 1_000);
    let l4 = env.create_portfolio(&l4o, 1_000);
    let s1 = env.create_portfolio(&s1o, 2);
    let s2 = env.create_portfolio(&s2o, 1_000);
    let s3 = env.create_portfolio(&s3o, 1_000);
    let s4 = env.create_portfolio(&s4o, 1_000);

    for (long_owner, long, short_owner, short) in [
        (&l1o, l1, &s1o, s1),
        (&l2o, l2, &s2o, s2),
        (&l3o, l3, &s3o, s3),
        (&l4o, l4, &s4o, s4),
    ] {
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
                    asset_index: 0,
                    size_q: percolator::POS_SCALE as i128,
                    exec_price: 1,
                    fee_bps: 0,
                },
            ),
            &[long_owner, short_owner],
        )
        .expect("ordinary users open one matched lot");
    }

    let oracle = env.oracle.insecure_clone();
    for (slot, mark) in [(101, 2), (102, 3), (103, 4), (104, 5)] {
        env.set_slot(slot);
        env.send(
            pix(
                vec![
                    AccountMeta::new(oracle.pubkey(), true),
                    AccountMeta::new(env.market, false),
                ],
                PIx::PushAuthMark {
                    asset_index: 0,
                    now_slot: slot,
                    mark_e6: mark,
                },
            ),
            &[&oracle],
        )
        .expect("independent oracle advances the normal mark");
        env.crank(s2, slot, true);
    }

    for portfolio in [s1, l1, l2, l3, l4, s2, s3, s4] {
        let _ = env.try_crank(portfolio, 104, false);
    }
    for _ in 0..8 {
        if active_leg(&portfolio_state(&env, s1)).is_none() {
            break;
        }
        env.try_crank(s1, 104, false)
            .expect("public liquidation closes the two-atom bankrupt short");
    }

    let (_, after_bankruptcy) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    assert_eq!(after_bankruptcy.mode, percolator::MarketModeV16::Live);
    assert!(active_leg(&portfolio_state(&env, s1)).is_none());
    assert_ne!(after_bankruptcy.assets[0].b_long_num, 0);
    assert_eq!(
        after_bankruptcy.assets[0].oi_eff_long_q,
        3 * percolator::POS_SCALE
    );
    assert_eq!(
        after_bankruptcy.assets[0].oi_eff_short_q,
        3 * percolator::POS_SCALE
    );

    settle_social_loss(&mut env, l1, 104);
    let l1_leg = active_leg(&portfolio_state(&env, l1)).unwrap();
    assert_eq!(l1_leg.b_rem, percolator::SOCIAL_LOSS_DEN / 2);
    let first_exit = try_reduce(&mut env, &l1o, l1);
    first_exit
        .as_ref()
        .expect("the first half-atom winner exits normally");
    assert!(active_leg(&portfolio_state(&env, l1)).is_none());

    let (_, after_first) =
        percolator_prog::state::read_market(&env.svm.get_account(&env.market).unwrap().data)
            .unwrap();
    assert_eq!(
        after_first.assets[0].social_loss_dust_long_num,
        percolator::SOCIAL_LOSS_DEN / 2,
    );

    settle_social_loss(&mut env, l2, 104);
    let l2_before = portfolio_state(&env, l2);
    let l2_leg = active_leg(&l2_before).unwrap();
    assert_eq!(l2_leg.b_rem, percolator::SOCIAL_LOSS_DEN / 2);
    let market_before_second = env.svm.get_account(&env.market).unwrap();
    let portfolio_before_second = env.svm.get_account(&l2).unwrap();
    let second_exit = try_reduce(&mut env, &l2o, l2);
    if second_exit.is_err() {
        assert_eq!(
            env.svm.get_account(&env.market).unwrap(),
            market_before_second
        );
        assert_eq!(env.svm.get_account(&l2).unwrap(), portfolio_before_second);
    }

    let mut successful_retry_cranks = 0u64;
    let mut successful_retry_reductions = 0u64;
    if active_leg(&portfolio_state(&env, l2)).is_some() {
        for slot in 105..=120 {
            env.set_slot(slot);
            if env.try_crank(l2, slot, false).is_ok() {
                successful_retry_cranks += 1;
            }
            if try_reduce(&mut env, &l2o, l2).is_ok() {
                successful_retry_reductions += 1;
                break;
            }
        }
    }

    let final_l2 = portfolio_state(&env, l2);
    let final_leg = active_leg(&final_l2);
    assert!(
        second_exit.is_ok() || successful_retry_reductions != 0,
        "the second half-atom carry trapped an ordinary owner: second_exit={second_exit:?}, successful_retry_cranks={successful_retry_cranks}, successful_retry_reductions={successful_retry_reductions}, capital={}, pnl={}, b_rem={:?}",
        final_l2.capital.get(),
        final_l2.pnl.get(),
        final_leg.map(|leg| leg.b_rem),
    );
    assert!(
        final_leg.is_none(),
        "the second winner must reach flat through a bounded public owner path"
    );
}
