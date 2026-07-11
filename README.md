# Percolator Meta

A non-custodial governance bootstrap and continuous reward system for Percolator markets.
Participants put base units at risk in market insurance during a short deposit window, run a
configurable bootstrap (six 30-day months by default), and vote on allocation of 100% of a fixed,
pre-existing COIN supply. After sealing, the votes have no further authority and depositors can
recover their owner-bound share of the insurance pool, subject to losses incurred during their
capital's own tenure.

> **Status:** experimental, educational-use-only, and provided **AS IS**. Participants can lose
> capital to market losses or program defects. See [LICENSE](LICENSE).

## Invariants

- **Fixed supply.** Genesis allocates an existing COIN vault. It never mints. Unclaimed genesis
  allocations are burned. A proposal becomes votable only after every declared entry slot is
  filled, so its creator cannot mutate it after voters lock capital. A claim can pay only an
  initialized COIN account owned by its signing recipient.
- **Capital stays segregated.** Insurance and backing principal stays in Percolator or an
  owner-bound subledger vault. Insurance haircuts and TWAP surplus are computed from the selected
  asset's own long/short domain budgets, so another asset's backing cannot mask a loss or authorize
  a withdrawal. Every subledger exit also requires an initialized token destination owned by the
  position owner. Ordinary exits require that owner's signature. After genesis is sealed and the
  bound market is resolved and empty, a fixed public return can retire only the complete position
  into a clean token account owned by that same depositor; it accepts no amount or beneficiary.
  Governance and reward programs custody COIN points/rewards only.
- **No governance withdrawal key.** Squads authorizes constrained program calls but does not hold
  a funded market's insurance operator, insurance authority, backing authority, or `asset_admin`.
  Controller-governed secondary activation requires the insurance authority and operator to be the
  same provider, so governance cannot invite an external deposit while retaining its withdrawal key.
  TWAP accepts only a real 1-of-1 Squads multisig whose sole all-permissions member and config
  authority are the named MetaDAO, with at least a one-week timelock.
- **Lifecycle is separate from custody.** The immutable `market-controller` PDA holds
  `marketauth` and can sign only a fixed allow-list of lifecycle, oracle, and fee-policy calls. Its
  generic proxy rejects live deposits, withdrawals, swaps, portfolio operations, and every
  authority mutation. Fixed permissionless paths return a secondary asset's remaining insurance
  only to the recorded insurance authority and its backing only to the recorded backing provider,
  both after shutdown matures and after a public stale resolver wins the race. Asset 0 instead has a
  fixed whole-market resolution path that derives and atomically returns both backing domains after
  Percolator proves the market empty. Controller-owned protocol insurance is recovered only to the
  canonical controller account after resolution and stays there until terminal close. Raw
  `CloseSlab` is excluded; a fixed terminal cleanup forwards that protocol insurance, vault dust,
  and account rent to Squads. Its controller-owned transit may be replaced by a token-empty account
  when an empty canonical ATA was permanently frozen; governance still signs, Percolator still
  requires a fully wound-down market, and the destination remains Squads-owned. Before that close,
  anyone can ask the controller to deregister an
  abandoned portfolio, but pinned Percolator accepts only a resolved market and an actually empty
  portfolio and returns its rent only to the slab. Historical reward counters cannot hold insurance
  or backing exits hostage: after any terminal dematerialization, any cranker can pay that LP/trader
  allocation from the frozen snapshot only to its bound recipient; lamports donated to the closed,
  zero-data witness cannot disable that recovery. Donating a creator-owned market
  preserves funded outgoing asset-0 insurance roles and the recorded backing provider; the handoff
  cannot collapse that capital into the controller or select another provider. `asset_admin` must
  migrate to the controller even on an empty market, unless read-only canonical Subledger or TWAP
  state proves that its fixed-cleanup PDA already owns admin and both insurance roles. This prevents
  a cold delegated admin from funding one atom after handoff and blocking terminal close. Donation
  is accepted only when every secondary slot is fully retired because Percolator does not migrate
  secondary `asset_admin` roles with `marketauth`;
  multi-asset markets are
  instead initialized under the controller before governance-approved activation. The handoff also
  requires direct permissionless asset append to be disabled, and the controller cannot enable it.
  Inbound bootstrap donations additionally require the controller to hold `marketauth`, both asset-0
  insurance roles, and asset-0 `asset_admin`, excluding any surviving key that could rotate and drain.
  A genesis-pool grant cannot rotate nonzero external insurance into pool custody: its recorded owner
  exits first, after which the same grant can proceed.
- **Custody transitions are fixed.** Asset-0 custody moves
  `market-controller -> genesis pool -> TWAP PDA`. The pool-to-TWAP handoff atomically imports the
  pool's live `outstanding_principal` as a minimum floor. TWAP also re-reads `marketauth` and accepts
  custody only before lifecycle donation, while its own Squads vault still controls the market, or
  after donation to the controller derived from that same vault. The floor can only rise. Recovery can
  return custody only to that same pool. Squads may authorize a live return, but a current-layout
  pool-bound depositor does not depend on it: the signing owner can atomically return custody and
  redeem their complete remaining position through the ordinary owner-bound Subledger checks. A
  failed or redirected exit rolls the role return back; success consumes that position and creates
  one exact-config re-handoff permit. Any cranker can consume that permit, replacing only the exited
  principal component of the floor. It cannot initiate the first handoff or undo a governance or
  terminal return. After the bound Percolator market is resolved and empty, anyone can crank the
  same fixed return while the pool attests that owner principal remains. Once genesis is sealed, an
  absent owner also cannot veto terminal cleanup: any cranker may
  return that owner's full loss-adjusted position only to a clean account owned by the depositor,
  after the real market is resolved and empty. An empty pool cannot pull terminal protocol insurance
  back from TWAP. After that insurance is first recovered to the canonical controller account,
  anyone may return the now value-less roles to the same pool so its fixed wrapper can release an
  absent asset-0 backing provider. The pool-less
  compatibility handoff accepts only an empty asset-0 insurance balance; later value must enter
  through the inbound-only donation path. Current-layout pool-less handoffs persist that empty-state
  attestation, allowing any cranker to move later donated terminal insurance only to the canonical
  controller account after resolution. Historical unmarked configs cannot infer protocol ownership.
  If external asset-0 backing remains, a separate amountless TWAP wrapper invokes only the
  controller's provider-bound resolved return, preserving controller insurance while paying both
  backing domains to Percolator's recorded provider.
  For pool-bound custody, TWAP can move a retained terminal floor to that controller account only
  after the bound pool itself attests that no owner principal or shares remain.
- **Market risk remains real.** Pool exits are pro rata under impairment. Governance can configure
  approved oracles and shut down or resolve markets, and oracle/market behavior can cause losses.
  Current insurance deposits are share-priced against loss-bearing principal on entry, so fresh
  capital does not recapitalize an older position's historical loss and protocol-surplus pulls do
  not look like depositor losses; governance still cannot redirect a depositor's withdrawal.

## Programs

| Crate | Responsibility |
|---|---|
| `percolator-accounting/` | Shared read-only parser for asset-local insurance balances/roles, backing authority/balances, and resolved-empty state in the pinned Percolator slab. It derives engine offsets from pinned layouts and exposes no instruction or authority. |
| `market-controller/` | Stateless, deny-by-default market lifecycle controller. Anyone can initialize a controller-owned market or donate an existing market authority. Squads can configure approved oracle modes, fee policies, asset lifecycle, shutdown, resolution, and atomically reclaim terminal protocol insurance/dust/rent from an empty slab. Its generic proxy cannot move insurance/backing, trade, rotate keys, or move portfolio collateral; fixed permissionless cleanup returns external insurance and backing only to their recorded providers, deregisters only empty resolved portfolios, and retains controller-owned protocol insurance for terminal reclaim. |
| `subledger/` | Owner-bound insurance/backing accounting. Genesis insurance pools bind market, Percolator program, COIN mint, policy, domain, deposit schedule, and bootstrap delay into the PDA. One base unit is one principal unit; priced shares keep losses scoped to each deposit's tenure while principal remains the vote/reward unit. Only a principal-policy pool can hand custody to TWAP. Its amountless full-exit entry point reuses the ordinary owner, pool, destination, loss, and share checks for an atomic live TWAP recovery. After recovery the pool can otherwise sign only fixed resolved cleanup. Once genesis-vote attests an executed winner and the bound market is resolved and empty, anyone can retire an absent depositor's complete position only into a clean account owned by that depositor. Its terminal read-only attestation proves when the pool has no owner claims. With-surplus pools retain direct owner redemption and cannot enter a protocol-surplus auction. |
| `genesis-vote/` | Bootstrap decider. Principal is the quorum denominator; support is weighted by `floor(log2(hold_time)) * principal`. One voter backs one proposal. Only a proposal with its complete declared entry shape can become votable. After the configured bootstrap deadline, a permissionless trigger seals the winner into `distribution`. Holds no funds. |
| `distribution/` | Claims from the fixed genesis COIN vault. A sealed proposal contains recipient/amount entries allocating at most the fixed supply. Each recipient authorizes its own claim; unclaimed or unallocated COIN is burned after the claim window. Never mints. |
| `residual-distributor/` | Reusable fixed or dynamic COIN reward epochs. It snapshots points from selected insurance/backing pools, realized residual flows, and cumulative funding paid (`long_paid + short_paid`, with no age multiplier), then pays only the position's bound recipient. Permissionless portfolio-flow claims reject delegated recipient accounts. It reads principal-bearing accounts but cannot debit them. |
| `twap-program/` | Post-genesis surplus auction and constrained asset-0 custodian. It can pull only insurance above the monotonic floor, run repeating uniform-price buybacks, burn or route bought COIN to a bound reward vault, accept inbound insurance donations, apply the exact trade/backing fee setters, and timelock-restart only an empty recovering asset 0. Pool-bound recovery stays bound to the original genesis pool; a signing owner may use its fixed live full-exit proxy, whose value accounts are all revalidated by Subledger. After resolution, either that pool proves zero claims or a current-layout pool-less config proves it began empty; TWAP can then route the terminal protocol floor only to the canonical market-controller account. A pool-less config can also sign only the controller's amountless, provider-bound asset-0 backing return. |
| `twap/` | Host-side auction simulation and Percolator wire helpers. It does not expose the obsolete raw Squads/operator rotation conveniences; deployed custody transitions live in `subledger` and `twap-program`. |
| `setup/` | Host helper for creating the fixed COIN supply and revoking mint authority. |

The workspace pins `percolator-prog` to commit
`186e9c6e189735ec43923dac7fcad1f8991a7ddd` and its engine layout dependency to
`143e68c4917ed0400a27b952f036a5677047cd84`.

## Genesis

1. **Create the market.** Anyone initializes a market with the governance-bound controller PDA as
   `marketauth`. Before user funding, Squads grants asset-0 custody to the canonical genesis pool;
   the only governance-held asset role is the oracle role.
2. **Deposit window.** Deposits are accepted only during
   `[bootstrap_start, bootstrap_start + deposit_window)`. The default window is about one week and
   cannot extend past bootstrap end. Topping up resets that position's age.
3. **Bootstrap vote.** The default bootstrap delay is six 30-day months and is configurable in the
   pool/config PDA. A voter must retract before changing proposals or withdrawing. Quorum and the
   winning weighted majority are computed from live, unwithdrawn principal. A proposal is not
   votable until its declared entry capacity is full, so no creator action remains after voting.
4. **Seal 100% of supply.** After the deadline, anyone can trigger a qualifying proposal. The
   winning allocation is immutable; recipients claim from the fixed vault and expired claims burn.
5. **Return deposits.** The intended genesis flow keeps custody in the owner-bound pool while
   initial risk takers redeem up to their loss-adjusted principal. If TWAP already holds custody,
   a signing owner can use the fixed atomic live path to return the roles and fully redeem their own
   remaining position without DAO cooperation. After resolution, returning the roles is
   permissionless while the bound market is empty and the pool still has owner principal. Sealed
   votes are no longer governance authority. If an owner disappears after sealing, any cranker can retire
   only that owner's full position into a clean token account owned by the depositor; the instruction
   has no caller-selected amount or beneficiary, so one atom cannot veto terminal market closure.
   Standalone with-surplus pools return their live share value pro rata but are never eligible for
   TWAP custody.
6. **Kickstart.** Futarchy can use retained surplus to initialize insurance/backing for approved
   markets and set fee splits. Inbound-only donation instructions fund Percolator without giving
   governance an insurance key and reject unless every asset-0 insurance withdrawal/rotation role is
   already the constrained controller.
7. **Handoff.** If capital remains for continuous operation, Squads authorizes the fixed
   principal-pool-to-TWAP transition. The transition records the exact pool identity and live
   principal in the TWAP floor. An atomic live owner exit creates a one-use permit for any cranker to
   re-handoff that same pool to that same config; the crank replaces only the exited principal
   component. Retained insurance stays protected and exited principal no longer strands fee surplus.
   Ordinary deposits and partial exits remain closed while TWAP holds custody.

## Continuous Rewards

TWAP rounds are externally cranked. Each round pulls only asset-0's live insurance surplus, retains
the configured insurance share by ratcheting the floor upward, and buys COIN at one marginal
clearing price. Whole-atom fills reconcile the USD payout to the integer COIN actually bought; every
executed pair must still satisfy both the bidder's limit and the DAO reserve, and fractional remainder
stays in the holding for a later round. A bid that cannot transact a whole-atom pair cannot reserve
nominal budget or set the marginal price. If every eligible bid is integer-infeasible, the aged book
settles for permissionless refunds without spending collateral. Insurance deposited for other assets
is never counted toward that surplus. Bought COIN can be split between burn and a canonical dynamic
reward vault.

The full chain test runs three 15-day rounds. Each round sends 50% of bought COIN to the reward
vault and burns 50%. At day 45 the accumulated reward vault distributes:

- 10% to selected insurance principal points;
- 10% to selected backing principal points;
- 80% to cumulative funding-payer points, summing long-paid and short-paid counters per portfolio.

Funding points have no age multiplier and receiver-side funding does not earn points. A portfolio
can earn from both its long-paid and short-paid totals. Insurance/backing reward points use live
base-unit principal times `floor(log2(tenure))`. Reward finalization and claims do not modify the
underlying principal, shares, or Percolator balances. Permissionless portfolio-flow claims pay only
to an initialized account owned by the bound recipient and reject any token delegate, so a cranker
cannot force rewards into an account it can spend.

Each reward epoch binds its authority, COIN mint, schedule, percentages, canonical vault, and up to
six selected market/pool scopes. A maximal six-scope initialization fits a one-member-signed Squads
transaction under the network packet limit. The DAO may configure future epochs but cannot mutate a live epoch,
redirect a user's claim, or sweep principal. After a sink epoch freezes, later rounds burn that stale
sink share rather than writing outside the snapshot. A closed or otherwise invalid exact-key COIN sink
also falls back to burn instead of stalling settlement.

Auction placement requires the bidder's canonical initialized COIN and collateral ATAs. Closed ATAs
are permissionlessly recreatable, and an already-frozen destination is rejected before any bid fee or
escrow transfer can move. If a freezable collateral issuer freezes the canonical ATA only after
placement, claims, eviction refunds, and owner cancellations may instead use a clean account for the
same mint owned solely by the recorded bidder. Delegates and close authorities are rejected at payout,
so a cranker can recover liveness but cannot change the beneficiary or expose the refund to a spender.

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
into insurance. Secondary activation may independently select backing and oracle providers, but its
insurance authority and operator must be one key so deposit and withdrawal custody cannot diverge.

Portfolio owners normally close their own empty accounts. Once a market is resolved, an absent owner
cannot hold `materialized_portfolio_count` above zero forever: any cranker can invoke the controller's
fixed portfolio cleanup. It signs only pinned Percolator `ClosePortfolio`; Percolator rejects live or
nonempty portfolios and sends the closed account's lamports only into the bound market slab.
Monotonic LP/trader/funding counters are reward telemetry, not a custody gate. If this cleanup or a
public maintenance sync dematerializes their exact linked portfolio, the residual distributor
still permits anyone to pay its frozen numerator only to the bound recipient, even if a third party
donates lamports to the closed address. The wrapper accepts no
amount, token account, or destination and exposes no generic portfolio authority.

An absent insurance authority or backing provider cannot block secondary-asset retirement with one
remaining atom. After Squads shuts the asset down and Percolator's delay and empty-state checks pass,
anyone can return its complete asset-local insurance and each backing domain through the controller.
The controller derives the insurance amount from the pinned slab and accepts no caller-selected
amount. Each destination must be a clean token account owned solely by the recorded provider, not a
DAO-selected beneficiary; this lets a cranker bypass a permanently frozen ATA without redirecting
funds. The temporary receiving account is likewise replaceable but must be a clean same-mint account
owned solely by the controller PDA. Backing earnings are paid first and the controller forwards
exactly the value attributed to that provider. It closes the temporary controller account only when no unrelated balance
remains. Protocol insurance or token dust already held there stays in controller custody for the
fixed terminal reclaim. While live, Percolator authorizes these returns only through its delayed
secondary-asset shutdown override.

Global stale resolution is permissionless, so a cranker can resolve before those shutdown returns
run. The resolved companions require the whole market to be resolved and empty, derive every amount
from the slab, and use the controller's existing secondary `asset_admin` role to rotate only the
relevant insurance or backing role. They then return all value to a clean account owned solely by
the outgoing recorded provider, but only up to the exact amount attributed by that cleanup. The caller and DAO
still choose neither an amount nor a recipient, and any failed rotation, withdrawal, forwarding, or
close rolls the entire operation back.

If the slab instead records the controller itself as insurance authority, including for asset 0,
the resolved companion withdraws only the exact asset-local amount into the canonical controller
account and leaves it there. It cannot select another destination. The balance reaches Squads only
through the existing governance-signed terminal reclaim after Percolator accepts `CloseSlab`.

The same terminal custody applies to TWAP-retained insurance after all genesis owners exit. The
subledger program first attests that its bound principal pool has no outstanding principal or
shares; only then can a permissionless TWAP crank route the exact resolved asset-0 remainder through
canonical TWAP and controller accounts. No caller or governance proposal selects the recipient. Once
the slab's asset-0 insurance is zero, a public role-only return to the same pool is safe and lets its
existing fixed wrapper complete asset-0 backing cleanup without a surviving DAO.

Asset 0 has no per-asset shutdown override. After whole-market resolution, its separate fixed path
reads both domains' complete principal and earnings from the pinned slab, atomically transfers the
backing role to the controller, and returns the full value to a clean account owned solely by the
outgoing provider. Before genesis custody moves, the controller is the constrained asset admin; after TWAP,
custody first returns to the canonical pool, whose only backing action is invoking this same fixed
cleanup. A failed domain CPI rolls back every earlier transfer and the authority change.

Permissionless market donation transfers lifecycle control, not funded creator capital. If the
outgoing market authority still owns nonzero asset-0 insurance, the controller atomically restores
that exact insurance authority/operator after accepting `marketauth`, just as it preserves the
recorded backing provider. A funded handoff that restores an outgoing insurance role or nonzero
backing bucket is rejected unless `asset_admin` migrates to the controller. The only non-migrating
exception requires the existing handoff to include canonical current-layout Subledger or TWAP state
bound to this exact market and Percolator program; both insurance roles must name the same constrained
PDA. An arbitrary delegated admin is rejected even while empty because it could fund after handoff.
Thus public stale resolution cannot make terminal recovery depend on a delegated signer.
If the provider disappears after a valid handoff, the controller's amountless resolved return rotates
only that value role and pays the complete asset-0 balance to a clean token account owned solely by the provider.
Genesis custody cannot move to a pool until those external insurance roles hold no balance. The
creator can also withdraw directly through Percolator, then the unchanged grant path installs the
canonical owner-bound pool; governance cannot convert the external balance into its own or
pool-controlled insurance. Percolator's market-authority update does not migrate secondary
`asset_admin` roles, so the controller rejects donation while any secondary slot is active,
drain-only, or recovering. Once those slots are empty and retired, the same permissionless handoff
succeeds only if direct permissionless asset append is disabled. The controller proxy cannot enable
that mode because a direct activator becomes an external `asset_admin`. New multi-asset deployments
use permissionless controller initialization followed by governance-approved asset activation, which
still assigns external insurance, backing, and oracle roles while keeping lifecycle admin constrained.
The same constrained proxy can restart an empty recovering asset through Percolator's value-neutral
restart instruction; it cannot choose a recipient or move insurance/backing while doing so.
Here, empty means every Percolator position, funding, loss, spent-budget, backing, and reservation
ledger is zero, not only zero OI. A previously traded slot with residual K/F accumulators cannot use
restart and must complete terminal recovery before governance initializes a fresh controller market.

At genesis-pool grant, the controller moves the oracle role to Squads and then atomically moves both
insurance roles and `asset_admin` to the pool. Squads may self-rotate the oracle role to an approved
builder, but it cannot use that role to move insurance or backing. Post-genesis, TWAP receives both
insurance roles and `asset_admin`; its exposed Percolator CPIs are fixed-purpose and accept no
arbitrary withdrawal destination. Governance therefore has no arbitrary resolved-mode insurance
withdrawal; external funds use the provider-bound fixed cleanup, while controller-owned protocol
insurance can move only to the canonical controller account for terminal reclaim.

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
