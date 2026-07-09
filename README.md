# Percolator Meta

A **non-custodial, Sybil-resistant governance bootstrap** for Percolator markets.
Depositors put capital at risk in a Percolator market's insurance to earn time-weighted
voting power over how a **fixed, pre-existing COIN supply** is distributed. The winning
distribution *is* the MetaDAO token; control of the market keys then transfers to it
through a time-locked Squads handover. Reward programs custody COIN only; depositor collateral
stays in Percolator or owner-bound subledger vaults.

> **Status.** Experimental, **educational-use-only**, provided **AS IS** with no warranties
> (see [LICENSE](LICENSE)). Participants put real capital **at risk** in a live market and
> can lose it to market losses.

## Premise

- **Depositing stakes capital to earn voting power.** The cost of a vote is the capital put
  at risk in the market's insurance — that real downside is what makes votes expensive to
  Sybil. The genesis pool is **share-based**: on exit a depositor redeems shares at the live
  insurance balance, recovering principal **plus a pro-rata slice of any market surplus**, and
  **pro-rata less under a loss**. A deployer may instead configure a **principal-only** pool
  (`POLICY_PRINCIPAL`), where surplus is retained in insurance as the DAO's buy/burn fuel.
  Because surplus is distributable it is also **winnable** — a deliberate governance tradeoff:
  it gives participants an incentive to compete for a winning COIN distribution.
- **COIN is a fixed supply with no mint authority.** Genesis does not mint — it *allocates* a
  pre-existing pool. No inflation, dilution, or mint-to-drain vector exists.
- **The DAO cannot take a user's principal.** User capital lives in Percolator insurance, never
  in a genesis-owned vault. The genesis programs do attribution/accounting only; the one path to
  unconstrained authority is a key rotation that runs through a **1-week Squads timelock**, giving
  every depositor a pre-announced window to exit first.
- **Post-genesis rewards reuse the same points engine.** Each immutable reward epoch selects up to
  six DAO-vetted markets and their insurance/backing pools. TWAP may send bought COIN into the
  epoch's canonical vault; the points engine can only read principal-bearing accounts and transfer
  COIN to bound recipients.

## Modules

How they fit: a depositor's stake is recorded by **subledger**. The proposal path uses
**genesis-vote + distribution**; the deterministic path uses **residual-distributor** directly.
Post-genesis, **Squads** rotates the market's insurance authority to **twap-program**, which can
split bought COIN between burn and another residual-distributor reward epoch.

| Crate | Role |
|---|---|
| `subledger/` | The market's **asset-0 insurance operator** during genesis (a role granted by Squads). Mediates deposits (signs Percolator `TopUpInsurance` as the insurance authority) and owner-authorized exits, tracking per-owner attribution (`owner, principal, start_slot`). Genesis pool PDAs bind the market, COIN mint, policy, domain, deposit window, and bootstrap start/delay. The pool and vote config therefore share one immutable schedule, and the deposit window cannot outlive bootstrap. Genesis pools are **share-based** (ERC4626-style): exiting redeems shares at the live balance, returning principal **plus any surplus**, pro-rata under loss. Never rotates keys — `accept_operator` only *consents* to receive the role Squads grants. Also provides reusable owner-bound pools for assets 1..N. |
| `genesis-vote/` | The **vote decider**. Runs a log-time quorum vote weighted by each voter's subledger attribution (`floor(log2(hold_time)) × principal`), one voter → one proposal. After the configured bootstrap schedule (`start_slot + delay`; default delay: six 30-day months), seals the winner into `distribution` by CPI. The config PDA commits to the schedule, so a permissionless first writer cannot shorten or early-start the clock. Holds no funds. |
| `residual-distributor/` | Reusable deterministic **COIN reward epochs**. Fixed mode distributes the whole genesis mint; dynamic mode snapshots COIN accumulated from TWAP. Both reuse `register → crystallize → freeze → claim` across log-time-weighted insurance/backing base-unit principal, LP/trader residual, and cumulative funding-payer (`long_paid + short_paid`, no age multiplier) cohorts. |
| `distribution/` | Holds the fixed COIN pool in a vault. A proposal is one incrementally grown on-chain account of up to ~10k `(pubkey, amount)` entries; the sealed winner's recipients **claim** permissionlessly, **unclaimed is burned**. Never mints. The `authority` (whichever decider PDA) is bound into the config seed, making it decider-pluggable. |
| `twap-program/` | Deployable BPF for the **authority chain** and post-mint uniform-price auction. It pulls only insurance *surplus*, clears at one marginal price, and splits bought COIN between burn and a DAO-pinned sink such as a reward-epoch vault. Never reaches the configured principal floor. |
| `twap/` | Reference library for the buy/burn (schedule + bid book); only its overflow-safe rate comparator is reused on-chain. |
| `setup/` | Host-side helper: init the fixed-supply 42M COIN mint and revoke the mint authority. |

The deterministic `residual-distributor` genesis path is tested with a 10% insurance / 10% backing /
80% cumulative funding-payer split. Funding-payer points are per portfolio:
`funding_long_paid_atoms_total + funding_short_paid_atoms_total`, with no age multiplier.

## Lifecycle

1. **Deposit window** — during `[bootstrap_start_slot, bootstrap_start_slot + deposit_window_slots)`,
   deposit through the subledger into market-0 insurance. It signs the Percolator top-up and records
   `owner, principal, start_slot` (last-write-time, so topping up resets the vote clock). The default
   window is about one week; its end must be no later than bootstrap end. Stake is held as shares.
2. **Vote** — `genesis-vote` reads the attribution. Weight = `floor(log2(hold_time)) × principal`,
   resolved at vote time. Backing a different proposal requires retracting first.
   Quorum = `total_voted_principal × 2 > outstanding`; winner = `support_weight × 2 > total_cast_weight`.
3. **Exit / veto (any time)** — redeem shares through the subledger for principal + surplus (pro-rata
   under loss). A live voter must **retract first**; the vote-lock blocks any withdraw not preceded by
   a retract, so a voter exits via a single atomic `[retract, withdraw]` transaction. Exiting shrinks
   `outstanding`, recomputing quorum against whoever stays — *those who stay decide*. This makes leaving
   the depositor's veto on a capture attempt.
4. **Trigger (permissionless, after bootstrap)** — once the configured `bootstrap_start_slot +
   bootstrap_delay_slots` has elapsed, the first proposal to clear quorum + weighted majority is sealed
   into `distribution` by CPI. No mint.
5. **Claim / burn** — winning recipients claim their entry from the fixed COIN vault; unclaimed is burned.
6. **Handoff (post-mint)** — control rotates `DAO → Squads (1-week timelock) → twap-program → Percolator`;
   the insurance authority moves from the principal-safe subledger to the surplus-only twap-program.
7. **Buy/burn (permissionless, repeating)** — each round, anyone `place_bid`s (escrow COIN, offer it for
   USD at a limit rate; a flat anti-spam fee is burned per bid). Bids can't be yanked to spoof a pending
   execute — early they can only be evicted by a strictly-better bid. When the round's slots expire,
   anyone calls `execute`: it pulls the burn-share (DAO-set, default 80%) of the current surplus, ratchets
   the retained share into the protected principal counter (so it compounds in insurance), clears the book
   at one marginal price (every winner pays the same; better bidders give less COIN, surplus refunded),
   and splits bought COIN between burn and a DAO-configured sink. Winners then `claim` their USD.
8. **Reward epoch (self-service)** — a DAO-authorized config immutably binds `(authority, COIN mint,
   epoch id)`, schedule, cohort bps, canonical COIN vault, and market/pool set. At the deadline,
   point crystallization closes; after the finalize delay, `freeze` snapshots the vault balance and
   cohort denominators, and users claim their own pro-rata COIN.

## Reward-fund boundary

`residual-distributor` has no instruction that withdraws insurance, backing, or portfolio collateral.
Those accounts are always read-only. Its only token CPI transfers the configured COIN mint from the
epoch PDA's canonical vault to the stake's pre-bound recipient. The DAO can choose future market scopes
and reward percentages, but cannot mutate an active epoch, redirect a claim, or sweep user principal.
Each subledger pool may belong to only one capital cohort in an epoch, so one position cannot claim both
the insurance and backing allocations through cross-market scope aliasing.

The full LiteSVM chain test runs fixed-supply genesis, then three consecutive 15-day TWAP rounds into
one dynamic reward epoch. Every round sends 50% of bought COIN to the epoch vault, burns 50%, pays the
seller, and reopens the book. At day 45 the cumulative vault pays 10% insurance, 10% backing, and 80%
cumulative funding-payer points across a DAO-selected market set. Insurance/backing balances and
attribution remain unchanged by reward finalization and claims. The book binds the epoch's final sink
slot: later permissionless rounds burn the stale sink share instead of depositing COIN outside the
frozen reward snapshot. Their reward points are live base-unit
principal times `floor(log2(tenure))`; this is comparable across selected pools and independent of each
pool's share price. Because a subledger top-up resets the position clock, late capital cannot inherit an
early registration's tenure. Capital pools in one epoch must use the same underlying base-unit
denomination; heterogeneous collateral belongs in separate epochs. Percolator oracle/crank maintenance
is external.

## Authority chain & the 1-week timelock

`DAO → Squads (1/1, 1-week timelock) → {subledger | twap-program} → Percolator`.

A program-created [Squads v4](https://squads.so) 1/1 multisig holds the market's asset-0 `asset_admin`
and is the **sole key-rotator**. At genesis it grants the insurance operator role to the subledger; post-mint
it rotates it to the twap-program — both via `UpdateAssetAuthority{asset_index:0}`, and Percolator requires the
incoming key to co-sign (the powerless `accept_operator` hooks). Every power-expanding rotation passes through
the one-week timelock, in the clear, with the old constrained authority still live — which is the user-exit
backstop: it bounds the blast radius of *any* bug in genesis-vote/distribution/chain to "users get a one-week,
pre-announced exit window."

## Market allow-list (residual-distributor)

The portfolio-flow cohorts award points from Percolator portfolio counters that **anyone who controls a
market's oracle can manufacture** (stand up an auth-mark market, self-trade delta-neutral, push funding/marks).
So a portfolio counts only if its market is on an orchestrator-vetted allow-list of trusted-Pyth markets
(`market_group` + up to 9 extras for legacy configs). A reward epoch atomically binds up to six
`(market, insurance pool, backing pool)` scopes; a market may be OI-only. Cohort 2 is LP residual received; cohort 3 is trader
residual loss net of spent principal; cohort 4 is the cumulative funding-payer counter:
`funding_long_paid_atoms_total + funding_short_paid_atoms_total`, with no age multiplier. Receiver-side funding counters do not earn points.
**Setup:** the creator holds the markets' authority key locally, stands up and vets N Pyth markets, then transfers
that key to the PDA that rotates it to the DAO via the same 1-week Squads timelock — so listed markets can never
be repointed at an attacker oracle once points accrue. The allow-list bounds *who can mint points at all*, not
wash-farming among already-trusted markets; see `residual-distributor/DESIGN.md` and `sim/` for that analysis.

## Build & test

```bash
# build the deployable BPF programs (each self-contained)
PERCOLATOR_MANIFEST="$(cargo metadata --format-version=1 | jq -r '.packages[] | select(.name=="percolator-prog") | .manifest_path')"
CARGO_TARGET_DIR="$PWD/target/percolator-prog-pinned" cargo build-sbf \
  --manifest-path "$PERCOLATOR_MANIFEST" \
  --sbf-out-dir "$PWD/target/deploy" \
  --no-default-features
cargo build-sbf --manifest-path subledger/Cargo.toml
cargo build-sbf --manifest-path distribution/Cargo.toml
cargo build-sbf --manifest-path genesis-vote/Cargo.toml
cargo build-sbf --manifest-path residual-distributor/Cargo.toml
cargo build-sbf --manifest-path twap-program/Cargo.toml

# tests (RUST_MIN_STACK is needed for the deep nested-CPI e2e)
RUST_MIN_STACK=8388608 cargo test --manifest-path subledger/Cargo.toml
RUST_MIN_STACK=8388608 cargo test --manifest-path genesis-vote/Cargo.toml
cargo test --manifest-path distribution/Cargo.toml
RUST_MIN_STACK=8388608 cargo test --manifest-path twap-program/Cargo.toml

# whole lifecycle across all six real binaries in one litesvm instance —
#   deposit -> vote -> distribute -> claim -> DAO/Squads handoff -> buy/burn auction:
RUST_MIN_STACK=8388608 cargo test --manifest-path twap-program/Cargo.toml \
    --test chain e2e_full_genesis_to_buy_burn

# deterministic genesis -> three 15-day TWAP 50/50 rounds -> cumulative 10/10/80 claims:
RUST_MIN_STACK=8388608 cargo test --manifest-path twap-program/Cargo.toml \
    --test chain e2e_market_genesis_traders_residual_decider_then_handoff_twap
```

Tests load the **real** binaries (the Cargo-pinned Percolator SBF at `target/deploy/percolator_prog.so`,
real Squads v4 at `program/tests/fixtures/squads_v4.so`, plus the locally-built crates) — CPIs run against
the actual programs, not mocks. The e2e needs those `.so` files prebuilt.

## License

[Apache License 2.0](LICENSE). Provided "as is", educational use only — see the disclaimer above.
