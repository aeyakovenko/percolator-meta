# residual-distributor — reusable reward epochs

Deterministic, points-based COIN rewards. The same program handles fixed-supply genesis and
post-genesis TWAP-funded epochs. It supports five cohorts, each split pro-rata to points:

- insurance deposits       (subledger share value; exit forfeits points)
- backing deposits         (subledger share value; exit forfeits points)
- LP residual received     (`residual_received_atoms_total` delta, log-time weighted)
- trader residual losses   (`residual_crystallized_loss_atoms_total - residual_spent_principal_atoms_total`, log-time weighted)
- cumulative funding payers (`funding_long_paid_atoms_total + funding_short_paid_atoms_total` delta, no age multiplier; optional bps)

## Reusable epoch model

`IX_INIT_REWARD_EPOCH` creates a canonical config PDA for
`(authority, COIN mint, epoch id)`. The authority signs only creation; the schedule, bps, canonical
COIN vault, and up to six `(market, insurance pool, backing pool)` scopes are immutable afterward.
The regular `register -> crystallize -> freeze -> claim` instructions are shared with genesis.

- **Fixed mode:** `expected_reward_supply > 0`; freeze requires that amount to equal the immutable
  mint supply and be present in the vault. This is deterministic genesis.
- **Vault-balance mode:** `expected_reward_supply == 0`; freeze snapshots the canonical vault's
  actual balance. TWAP can send its retained share of bought COIN into that vault during the epoch.
- TWAP books that target an epoch vault bind an inclusive sink cutoff to the epoch end. A later
  permissionless round burns the would-be reward share, so no COIN can arrive outside the frozen
  snapshot and become permanently unclaimable.
- Registration is half-open `[start, end)` and crystallization closes at the inclusive `end` slot.
  The later finalize window delays permissionless freeze but cannot admit counter growth created
  after the reward period.
- A scope may omit insurance/backing pools and contribute only portfolio-flow points. This lets a
  handed-off genesis market provide capital cohorts while other DAO-vetted live markets provide OI.

No reward instruction can move collateral. Subledger positions and Percolator portfolios are
read-only inputs. The only token CPI transfers the configured COIN mint from the epoch PDA's bound
vault to the stake's bound recipient. A rewards bug can affect COIN allocation; it cannot withdraw,
redirect, lock, or confiscate insurance/backing principal.

## Anti-capture stack (defence in depth, weakest-to-strongest)

1. **Weak log-time weight** `floor(log2(hold))` — a late whale is only ~1.15x behind an early
   backer. Necessary but not sufficient.
2. **Funding-payer counters** — points are paid to the side that actually paid funding, never the receiving side.
   A self-owned payer/receiver pair can internalize the funding transfer, so these cohorts still pay the
   configured claim fee retained in the vault.
3. **Residual JIT damping** — LP/trader residual points use the farm-side hold window, so a 1-slot
   residual sniper earns ~0. Funding-payer points intentionally use only paid accumulator delta.
4. **SOFT VETO (the teeth).** Insurance runs POLICY_WITH_SURPLUS, no lock: a depositor may exit
   ANY TIME taking principal + pro-rata fee surplus, FORFEITING their COIN share. So if an
   attacker farms points to capture the COIN (and thus the surplus), honest insurance need not
   out-farm him — they EXIT WITH THE SURPLUS and he captures COIN over an empty pool. Capture is
   a Pyrrhic win: the point math makes farming expensive; the soft veto makes a successful farm
   WORTHLESS. Governance (COIN) is decoupled from value (surplus); the value can always walk.

## What the soft veto requires of this program

- An exited insurance position MUST forfeit its points: the seal must not allocate COIN to a
  position that has withdrawn. Mechanism: exit (subledger withdraw) invalidates the PointStake,
  or the seal cross-checks the live position and skips/zeros withdrawn ones. Forfeited COIN is
  not minted (floor rounding / unallocated supply is burned by distribution's burn_unclaimed).
- Symmetric for backing depositors if they exit before crystallization: live shares drop, so their points drop.

## Trust / determinism
- `IX_FREEZE` snapshots cohort denominators and the reward supply after the finalize window; `IX_CLAIM` then pays
  `floor(cohort_supply * stake.points / frozen_total_points)` to the stake's bound recipient.
  Nothing is trusted from a cranker; users self-claim their deterministic share.
- Stake PDAs are derived as `[b"rd_stake", config, owner, linked_account, reward_family]`. Insurance,
  backing, residual, and funding are distinct families; LP and trader deliberately share the residual
  family. One wallet can register multiple legitimate linked accounts, and one portfolio can earn both
  residual and paid-funding rewards without double-registering across LP/trader cohorts.
- Percolator stays subledger-free: this program snapshot-deltas its monotonic portfolio counters
  (LP/trader residual counters plus optional funding-paid counters). Offsets are pinned with
  `offset_of!` against the real Percolator structs.

## Status
- Done + unit/e2e-tested: point math (residual log2 / window / pro-rata); Config + Stake state;
  init / register_start / crystallize / freeze / claim.
- Done + e2e (real SBF binaries): funding-payer points go to cumulative paid funding
  (`funding_long_paid_atoms_total + funding_short_paid_atoms_total`), not `*_received_atoms_total`
  deltas, with no age multiplier. The tested genesis split is 10% insurance + 10% backing + 80% cumulative funding-payer.
  The chain test drives a live Percolator market through EWMA mark config, long-pays-short and
  short-pays-long funding, fixed-supply genesis payout, DAO handoff, three consecutive 15-day TWAP
  auctions, a per-round 50% reward / 50% burn split, and one cumulative 10/10/80 dynamic reward epoch
  across two selected markets.
- Done + unit-tested: SOFT-VETO forfeiture. `insurance_points(seal_slot, principal, start_slot,
  withdrawn)` reads the LIVE subledger position; a withdrawn / zero-principal position yields 0,
  so a depositor that exited with the surplus forfeits its COIN (the share is never allocated and
  is burned as unclaimed by distribution::burn_unclaimed). `read_subledger_position` reads the
  stable Position offsets (principal@72 / withdrawn@88 / start_slot@89).
- Done: share-value cohort path. Supply splits insurance/backing/LP/funding-payer explicitly
  and assigns the remainder to trader; insurance
  points = capital*log-time crystallized from the LIVE subledger position into an authoritative
  insurance_total_points (subtract-old/add-new); seal verifies each insurance entry against it AND
  reads the live position to FORFEIT (amount must be 0) a withdrawn depositor — the forfeited share
  stays in the total and is burned as unclaimed, never redistributed. e2e: insurance_cohort_split_and_exit_forfeiture.
- Done: Percolator portfolio residual and funding-counter offsets pinned with offset_of! (tests/offsets.rs).

## Market allow-list (portfolio-flow cohorts) — finding IL+

**Why an allow-list is necessary.** The LP/trader residual cohorts and funding-payer cohort award
points from percolator PortfolioAccount counters. Those counters are manufacturable by anyone who controls
the market/oracle path: stand up a market with an auth-mark/manual oracle, self-trade both sides, and create
residual or funding flow while internalizing the other side. So a portfolio is countable **only if its
provenance market is on an orchestrator-vetted allow-list of trusted-Pyth markets whose oracle the public
cannot move.**

**Config.** Legacy configs use `market_group` (primary) + up to `MAX_EXTRA_MARKETS` (9) extras.
Reward epochs atomically bind up to six full market/pool scopes; six is conservative under Solana's
transaction-size limit with two signatures. All scope fields are fixed at init.
`register_start` for the portfolio-flow cohorts requires `portfolio.provenance.market_group ∈ allow-list`
(`Config::market_allowed`). Pinned by allow-list e2e tests;
the single-market form is finding IL (`register_rejects_portfolio_from_a_foreign_market`).

**Setup flow (how the allow-listed markets are made trustworthy).** At genesis init the market-authority
key (the asset_admin / oracle authority of the N markets) is held **locally by the creator**. The creator
stands up the N markets, binds each to a real Pyth feed, vets them, and only THEN transfers that
market-authority key to the **PDA that rotates it onward to the DAO** via the Squads 1-week-timelock
handoff (subledger/twap `accept_operator` → percolator `UpdateAssetAuthority`). After the transfer the
allow-listed markets can no longer be repointed at an attacker oracle — their oracle authority lives
behind the timelock'd DAO, exactly like the insurance operator. The allow-list is therefore safe because
(a) only vetted markets are listed, and (b) their oracle authority is locked to the DAO before any
points accrue. **Operators MUST keep the allow-list to trusted-Pyth markets** — listing a market whose
oracle anyone can move re-opens the free-point attack.
