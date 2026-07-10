# Percolator Meta

A non-custodial governance bootstrap and continuous reward system for Percolator markets.
Participants put base units at risk in market insurance during a short deposit window, run a
configurable bootstrap (six 30-day months by default), and vote on allocation of 100% of a fixed,
pre-existing COIN supply. After sealing, the votes have no further authority and depositors can
recover their owner-bound share of the insurance pool, subject to market losses.

> **Status:** experimental, educational-use-only, and provided **AS IS**. Participants can lose
> capital to market losses or program defects. See [LICENSE](LICENSE).

## Invariants

- **Fixed supply.** Genesis allocates an existing COIN vault. It never mints. Unclaimed genesis
  allocations are burned. A claim can pay only an initialized COIN account owned by its signing
  recipient.
- **Capital stays segregated.** Insurance and backing principal stays in Percolator or an
  owner-bound subledger vault. Insurance haircuts and TWAP surplus are computed from the selected
  asset's own long/short domain budgets, so another asset's backing cannot mask a loss or authorize
  a withdrawal. Every subledger exit also requires an initialized token destination owned by the
  signing position owner. Governance and reward programs custody COIN points/rewards only.
- **No governance withdrawal key.** Squads authorizes constrained program calls but does not hold
  a funded market's insurance operator, insurance authority, backing authority, or `asset_admin`.
  TWAP accepts only a real 1-of-1 Squads multisig whose sole all-permissions member and config
  authority are the named MetaDAO, with at least a one-week timelock.
- **Lifecycle is separate from custody.** The immutable `market-controller` PDA holds
  `marketauth` and can sign only a fixed allow-list of lifecycle, oracle, and fee-policy calls. Its
  generic proxy rejects live deposits, withdrawals, swaps, portfolio operations, and every
  authority mutation. Fixed permissionless paths return a secondary asset's remaining insurance
  only to the recorded insurance authority and its backing only to the recorded backing provider,
  both after shutdown matures and after a public stale resolver wins the race. Asset 0 instead has a
  fixed whole-market resolution path that derives and atomically returns both backing domains after
  Percolator proves the market empty. Raw
  `CloseSlab` is excluded; a fixed terminal cleanup forwards only vault dust and account rent to
  Squads. Donating a creator-owned market preserves funded outgoing asset-0 insurance roles and the
  recorded backing provider; the handoff cannot collapse that capital into the controller or select
  another provider. Donation is accepted only when every secondary slot is fully retired because
  Percolator does not migrate secondary `asset_admin` roles with `marketauth`; multi-asset markets are
  instead initialized under the controller before activation. A genesis-pool grant cannot rotate
  nonzero external insurance into pool custody: its recorded owner exits first, after which the same
  grant can proceed.
- **Custody transitions are fixed.** Asset-0 custody moves
  `market-controller -> genesis pool -> TWAP PDA`. The pool-to-TWAP handoff atomically imports the
  pool's live `outstanding_principal` as a minimum floor. The floor can only rise. Recovery can
  return custody only to that same pool.
- **Market risk remains real.** Pool exits are pro rata under impairment. Governance can configure
  approved oracles and shut down or resolve markets, and oracle/market behavior can cause losses;
  it cannot redirect a depositor's withdrawal to itself.

## Programs

| Crate | Responsibility |
|---|---|
| `percolator-accounting/` | Shared read-only parser for asset-local insurance balances/roles, backing authority/balances, and resolved-empty state in the pinned Percolator slab. It derives engine offsets from pinned layouts and exposes no instruction or authority. |
| `market-controller/` | Stateless, deny-by-default market lifecycle controller. Anyone can initialize a controller-owned market or donate an existing market authority. Squads can configure approved oracle modes, fee policies, asset lifecycle, shutdown, resolution, and atomically reclaim terminal dust/rent from an empty slab. Its generic proxy cannot move insurance/backing, trade, rotate keys, or move portfolio collateral; fixed permissionless cleanup returns insurance and backing only to their recorded providers. |
| `subledger/` | Owner-bound insurance/backing accounting. Genesis insurance pools bind market, Percolator program, COIN mint, policy, domain, deposit schedule, and bootstrap delay into the PDA. One base unit is one principal unit; shares track tenure and live capital for rewards. Only a principal-policy pool can hand custody to TWAP; after recovery it can sign only the controller's fixed resolved asset-0 backing return, including for supported historical pool schemas. With-surplus pools retain direct owner redemption and cannot enter a protocol-surplus auction. |
| `genesis-vote/` | Bootstrap decider. Principal is the quorum denominator; support is weighted by `floor(log2(hold_time)) * principal`. One voter backs one proposal. After the configured bootstrap deadline, a permissionless trigger seals the winner into `distribution`. Holds no funds. |
| `distribution/` | Claims from the fixed genesis COIN vault. A sealed proposal contains recipient/amount entries totaling the fixed supply. Claims are permissionless; unclaimed COIN is burned after the claim window. Never mints. |
| `residual-distributor/` | Reusable fixed or dynamic COIN reward epochs. It snapshots points from selected insurance/backing pools, realized residual flows, and cumulative funding paid (`long_paid + short_paid`, with no age multiplier), then pays only the position's bound recipient. It reads principal-bearing accounts but cannot debit them. |
| `twap-program/` | Post-genesis surplus auction and constrained asset-0 custodian. It can pull only insurance above the monotonic floor, run repeating uniform-price buybacks, burn or route bought COIN to a bound reward vault, accept inbound insurance donations, and apply the exact trade/backing fee setters. Its recovery path is bound to the original genesis pool. |
| `twap/` | Host-side auction simulation and Percolator wire helpers. It does not expose the obsolete raw Squads/operator rotation conveniences; deployed custody transitions live in `subledger` and `twap-program`. |
| `setup/` | Host helper for creating the fixed COIN supply and revoking mint authority. |

The workspace pins `percolator-prog` to commit
`624b13da8ed96f49b6049a4874052e05ae7a7cb6` and its engine layout dependency to
`c8aab33814dd30878cf9b054eca89bcd6cf9f5e7`.

## Genesis

1. **Create the market.** Anyone initializes a market with the governance-bound controller PDA as
   `marketauth`. Before user funding, Squads grants asset-0 custody to the canonical genesis pool;
   the only governance-held asset role is the oracle role.
2. **Deposit window.** Deposits are accepted only during
   `[bootstrap_start, bootstrap_start + deposit_window)`. The default window is about one week and
   cannot extend past bootstrap end. Topping up resets that position's age.
3. **Bootstrap vote.** The default bootstrap delay is six 30-day months and is configurable in the
   pool/config PDA. A voter must retract before changing proposals or withdrawing. Quorum and the
   winning weighted majority are computed from live, unwithdrawn principal.
4. **Seal 100% of supply.** After the deadline, anyone can trigger a qualifying proposal. The
   winning allocation is immutable; recipients claim from the fixed vault and expired claims burn.
5. **Return deposits.** The intended genesis flow keeps custody in the owner-bound pool while
   initial risk takers redeem up to their loss-adjusted principal. If TWAP already holds custody,
   its fixed recovery path returns it to that same pool first. Sealed votes are no longer governance
   authority. Standalone with-surplus pools return their live share value pro rata but are never
   eligible for TWAP custody.
6. **Kickstart.** Futarchy can use retained surplus to initialize insurance/backing for approved
   markets and set fee splits. Inbound-only donation instructions fund Percolator without giving
   governance an insurance key.
7. **Handoff.** If capital remains for continuous operation, Squads authorizes the fixed
   principal-pool-to-TWAP transition. The transition records the exact pool identity and live
   principal in the TWAP floor. After a recovery, re-handoff replaces only that principal component;
   retained insurance stays protected and exited principal no longer strands fee surplus. Deposits
   and exits are closed while TWAP holds custody; the fixed recovery transition restores owner exits.

## Continuous Rewards

TWAP rounds are externally cranked. Each round pulls only asset-0's live insurance surplus, retains
the configured insurance share by ratcheting the floor upward, and buys COIN at one marginal
clearing price. Insurance deposited for other assets is never counted toward that surplus. Bought
COIN can be split between burn and a canonical dynamic reward vault.

The full chain test runs three 15-day rounds. Each round sends 50% of bought COIN to the reward
vault and burns 50%. At day 45 the accumulated reward vault distributes:

- 10% to selected insurance principal points;
- 10% to selected backing principal points;
- 80% to cumulative funding-payer points, summing long-paid and short-paid counters per portfolio.

Funding points have no age multiplier and receiver-side funding does not earn points. A portfolio
can earn from both its long-paid and short-paid totals. Insurance/backing reward points use live
base-unit principal times `floor(log2(tenure))`. Reward finalization and claims do not modify the
underlying principal, shares, or Percolator balances.

Each reward epoch binds its authority, COIN mint, schedule, percentages, canonical vault, and up to
six selected market/pool scopes. The DAO may configure future epochs but cannot mutate a live epoch,
redirect a user's claim, or sweep principal. After a sink epoch freezes, later rounds burn that stale
sink share rather than writing outside the snapshot. A closed or otherwise invalid exact-key COIN sink
also falls back to burn instead of stalling settlement.

## Authority Model

Governance and custody are deliberately different chains:

```text
DAO -> Squads (DAO-only member, minimum 1-week timelock) -> market-controller PDA -> Percolator marketauth

market-controller PDA -> owner-bound genesis pool -> config-bound TWAP PDA
                           asset custody             asset custody
```

Squads can make the controller sign only the exact pinned allow-list. The allow-list includes market
and asset lifecycle, approved oracle configuration, bounded fee policy, resolution, and empty-slab
cleanup. Raw `CloseSlab` is not proxied: the dedicated cleanup forwards its mandatory controller
destinations atomically. The proxy excludes deposits, withdrawals, swaps, portfolio operations,
authority rotation, and backing-bucket movement. External backing providers retain their own
asset-local withdrawal path; governance only sets the fee split that sends the configured share
into insurance.

An absent insurance authority or backing provider cannot block secondary-asset retirement with one
remaining atom. After Squads shuts the asset down and Percolator's delay and empty-state checks pass,
anyone can return its complete asset-local insurance and each backing domain through the controller.
The controller derives the insurance amount from the pinned slab and accepts no caller-selected
amount. Each destination is the recorded provider's canonical token account, not a DAO-selected
account; backing earnings are paid first and each temporary controller account is forwarded and
closed atomically. While live, Percolator authorizes these returns only through its delayed
secondary-asset shutdown override.

Global stale resolution is permissionless, so a cranker can resolve before those shutdown returns
run. The resolved companions require the whole market to be resolved and empty, derive every amount
from the slab, and use the controller's existing secondary `asset_admin` role to rotate only the
relevant insurance or backing role. They then return all value to the outgoing recorded provider's
canonical account. The caller and DAO still choose neither an amount nor a recipient, and any failed
rotation, withdrawal, forwarding, or close rolls the entire operation back.

Asset 0 has no per-asset shutdown override. After whole-market resolution, its separate fixed path
reads both domains' complete principal and earnings from the pinned slab, atomically transfers the
backing role to the controller, and returns the full value to the outgoing provider's canonical
account. Before genesis custody moves, the controller is the constrained asset admin; after TWAP,
custody first returns to the canonical pool, whose only backing action is invoking this same fixed
cleanup. A failed domain CPI rolls back every earlier transfer and the authority change.

Permissionless market donation transfers lifecycle control, not funded creator capital. If the
outgoing market authority still owns nonzero asset-0 insurance, the controller atomically restores
that exact insurance authority/operator after accepting `marketauth`, just as it preserves the
recorded backing provider. Genesis custody cannot move to a pool until those external insurance
roles hold no balance. The creator can withdraw through Percolator, then the unchanged grant path
installs the canonical owner-bound pool; governance cannot convert the external balance into its own
or pool-controlled insurance. Percolator's market-authority update does not migrate secondary
`asset_admin` roles, so the controller rejects donation while any secondary slot is active,
drain-only, or recovering. Once those slots are empty and retired, the same permissionless handoff
succeeds. New multi-asset markets should use permissionless controller initialization before assets
are activated.

At genesis-pool grant, the controller moves the oracle role to Squads and then atomically moves both
insurance roles and `asset_admin` to the pool. Squads may self-rotate the oracle role to an approved
builder, but it cannot use that role to move insurance or backing. Post-genesis, TWAP receives both
insurance roles and `asset_admin`; its exposed Percolator CPIs are fixed-purpose and accept no
arbitrary withdrawal destination. Governance therefore has no arbitrary resolved-mode insurance
withdrawal; only the provider-bound fixed cleanup is available.

## Build And Test

```bash
# Build the exact pinned Percolator binary used by LiteSVM.
PERCOLATOR_MANIFEST="$(cargo metadata --format-version=1 | jq -r \
  '.packages[] | select(.name=="percolator-prog") | .manifest_path')"
CARGO_TARGET_DIR="$PWD/target/percolator-prog-pinned" cargo build-sbf \
  --manifest-path "$PERCOLATOR_MANIFEST" \
  --sbf-out-dir "$PWD/target/deploy" \
  --no-default-features

cargo build-sbf --manifest-path market-controller/Cargo.toml
cargo build-sbf --manifest-path subledger/Cargo.toml
cargo build-sbf --manifest-path distribution/Cargo.toml
cargo build-sbf --manifest-path genesis-vote/Cargo.toml
cargo build-sbf --manifest-path residual-distributor/Cargo.toml
cargo build-sbf --manifest-path twap-program/Cargo.toml

cargo test --workspace

# Full real-binary genesis, long/short funding, handoff, three TWAP rounds,
# 50/50 buyback burn/reward routing, and cumulative 10/10/80 claims.
RUST_MIN_STACK=8388608 cargo test --manifest-path twap-program/Cargo.toml \
  --test chain e2e_market_genesis_traders_residual_decider_then_handoff_twap \
  -- --exact --nocapture
```

LiteSVM loads the Cargo-pinned Percolator SBF, real Squads v4 fixture, and locally built program
binaries. The end-to-end assertions exercise real CPIs rather than mocks. Oracle pushes and other
permissionless cranks are triggered externally in the test, matching the intended deployment.

## License

[Apache License 2.0](LICENSE). Provided as is for educational use.
