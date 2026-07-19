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
  position owner. Ordinary exits require that owner's signature. After the committed bootstrap
  deadline and once the bound market is resolved and empty, a fixed public return can retire only
  the complete position into a clean token account owned by that same depositor; it accepts no
  amount or beneficiary and does not depend on a proposal winning. The same transaction retires
  that owner's exact live ballot from Genesis tallies, so refunded votes are worthless.
  Governance and reward programs custody COIN points/rewards only.
- **No governance withdrawal key.** Squads authorizes constrained program calls but does not hold
  a funded market's insurance operator, insurance authority, backing authority, or `asset_admin`.
  Controller-governed secondary activation keeps the insurance authority and operator on the
  constrained controller, so governance cannot invite an external deposit while retaining a raw
  withdrawal key.
  TWAP accepts only a real 1-of-1 Squads multisig whose sole all-permissions member and config
  authority are the named MetaDAO, with at least a one-week timelock.
- **Immutable deployment is a precondition.** Before accepting deposits, revoke the upgrade
  authorities for Percolator and every Meta program whose PDA signs for custody or governance.
  A retained upgrade authority can replace these on-chain constraints and invalidates the
  no-withdrawal-key guarantee. Verify deployed program IDs, binaries, and ProgramData authorities
  against the audited commits; Squads and MetaDAO upgradeability remain explicit external trust
  assumptions. A collateral mint's freeze authority is equally explicit trust: it can freeze the
  canonical Percolator vault and stop every withdrawal, which no Meta program can override. A
  deployment claiming issuer-independent fund liveness must accept only collateral whose SPL Token
  freeze authority is revoked.
- **Lifecycle is separate from custody.** The immutable `market-controller` PDA holds
  `marketauth` and can sign only a fixed allow-list of lifecycle, oracle, and fee-policy calls. Its
  generic proxy rejects live deposits, withdrawals, swaps, portfolio operations, and every
  authority mutation. Fixed permissionless paths return a secondary asset's remaining insurance
  only to the recorded insurance authority and its backing only to the recorded backing provider,
  both after shutdown matures and after a public stale resolver wins the race. Asset 0 instead has a
  fixed whole-market resolution path that derives and atomically returns both backing domains after
  Percolator proves the market empty. Controller-owned protocol insurance is recovered through an
  empty one-shot controller transit directly to a clean Squads-vault-owned account. TWAP uses the
  same governance destination rule after proving no owner principal remains, so public cleanup
  cannot split protocol value across persistent controller accounts. Raw `CloseSlab` is excluded;
  a fixed terminal cleanup forwards vault dust, any pre-upgrade controller balance, and account rent
  to Squads after reserving reclaimed slab rent in an immutable market-retirement marker. Governance
  still signs, Percolator still requires a fully wound-down market, and the destination remains
  Squads-owned. Before that close,
  anyone can ask the controller to deregister an
  abandoned portfolio, but pinned Percolator accepts only a resolved market and an actually empty
  portfolio and returns its rent only to the slab. Before deleting nonzero LP/trader/funding-payer
  telemetry, the same atomic call checked-adds it into a residual-distributor PDA scoped to the
  Percolator program, market, owner, and portfolio. Reward epochs read archived totals plus any live
  account generation. Each stake also binds that market's immutable allow-list index, so a later
  same-key account generation in another market cannot replace or block the archived identity. Every
  portfolio-flow registration, counter read, and telemetry-bearing cleanup supplies the exact
  retirement marker: registration rejects retired keys, existing stakes read only their archive after
  retirement, and cleanup stops appending telemetry once the marker exists. Reinitializing a closed
  slab therefore cannot revive or inflate its reward eligibility.
  Controller initialization and authority acceptance also require that canonical marker and reject
  a retired key. Each slab key can therefore enter Meta governance for only one generation, so an
  approved but unexecuted Squads action cannot cross into a replacement market at the same address.
  Fresh slab keys remain permissionless, and direct Percolator reuse remains outside Meta custody.
  Terminal cleanup therefore cannot erase an uncrystallized allocation or hold insurance and backing
  exits hostage. Portfolio-flow registration also authenticates the matching market and
  requires its immutable account maintenance fee to be zero, so direct permissionless fee sync cannot
  erase counters before they reach the archive. The archive has no token or value-moving instruction.
  Before donating a creator-owned market, its raw
  outgoing asset-0 insurance provider must withdraw any nonzero balance. The empty insurance roles
  then migrate to the controller, while the recorded backing provider remains unchanged. This keeps
  a former provider from withdrawing its principal yet retaining a claim on later trade fees.
  `asset_admin` must
  migrate to the controller even on an empty market, unless read-only canonical Subledger or TWAP
  state proves that its fixed-cleanup PDA already owns admin and both insurance roles. This prevents
  a cold delegated admin from funding one atom after handoff and blocking terminal close. Insurance
  authority and operator must also be one identity even while empty, and that identity must be the
  consenting outgoing authority, the controller, or a canonical constrained custodian. Otherwise an
  unfunded third party could collect later public trade fees without putting capital at risk. Donation
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
  terminal return. An ordinary pool exit first proves the pool is still the live insurance operator,
  even when full impairment makes its token payout zero, so it cannot change principal behind TWAP.
  A positive live payout additionally requires asset 0 to have no open position accounting or
  unresolved loss state. This prevents a stale losing portfolio from realizing its loss only after
  the reserve has left; a fully impaired zero-payout position can still retire.
  After the bound Percolator market is resolved and empty, anyone can crank the
  same fixed return while the pool attests that owner principal remains. Once the bootstrap deadline
  has elapsed, an absent owner also cannot veto terminal cleanup: any cranker may
  return that owner's full loss-adjusted position only to a clean account owned by the depositor,
  after the real market is resolved and empty. That fixed return records the principal still at risk
  and its return slot, so a cranker cannot erase an uncrystallized capital reward, exited capital
  cannot keep aging, and earlier owner withdrawals stay excluded.
  An empty pool cannot pull terminal protocol insurance
  back from TWAP. After that insurance is first recovered to a clean Squads-vault-owned account,
  anyone may return the now value-less roles to the same pool so its fixed wrapper can release an
  absent asset-0 backing provider. The pool-less
  compatibility handoff accepts only an empty asset-0 insurance balance; later value must enter
  through the inbound-only donation path. Current-layout pool-less handoffs persist that empty-state
  attestation, allowing any cranker to move later donated terminal insurance through an empty
  TWAP-owned transit only to a clean Squads-vault-owned account after resolution. Historical
  unmarked configs cannot infer protocol ownership.
  If external asset-0 backing remains, a separate amountless TWAP wrapper invokes only the
  controller's provider-bound resolved return, preserving controller insurance while paying both
  backing domains to Percolator's recorded provider.
  For pool-bound custody, TWAP can return a retained terminal floor to the same fixed governance
  owner only after the bound pool itself attests that no owner principal or shares remain.
- **Market risk remains real.** Pool exits are pro rata under impairment. Governance can configure
  approved oracles and shut down or resolve markets, and oracle/market behavior can cause losses.
  Current insurance deposits are share-priced against loss-bearing principal on entry, so fresh
  capital does not recapitalize an older position's historical loss and protocol-surplus pulls do
  not look like depositor losses. A deposit whose newly minted shares would be worth more than one
  base unit less than the deposit at the post-deposit price rejects before transfer, so virtual-share
  rounding cannot silently consume material principal. Each exit rounds only that position's claim
  down; enough unowned pricing shares remain to prevent a later exit from collecting the remainder.
  Odd atoms are split against the pool-wide 50/50 target instead of per deposit. Live exits reverse
  that principal tranche atomically, so a temporary depositor cannot redirect another owner's
  long/short loss protection by withdrawing. TWAP surplus rounds use the same reservation-aware
  planner: auction and savings pulls leave the ratcheted floor at the canonical 50/50 domain split
  instead of consuming one side's principal behind an aggregate floor. A zero-surplus round also
  repairs the exact imbalance left by a predecessor TWAP deployment without moving net value.
  While asset 0 has open positions or unresolved loss state, rounds continue to settle and advance
  but pull no fresh insurance and leave the floor unchanged.
  Any final whole-atom remainder stays protocol insurance. Governance still cannot redirect a
  depositor's withdrawal.

## Programs

| Crate | Responsibility |
|---|---|
| `percolator-accounting/` | Shared read-only parser for asset-local generation IDs, insurance balances/roles, position-or-loss withdrawal blockers, backing authority/balances, resolved-empty state, and portfolio reward telemetry in the pinned Percolator layout. It derives engine offsets from pinned layouts and exposes no instruction or authority. |
| `market-controller/` | Stateless, deny-by-default market lifecycle controller. Anyone can initialize a fresh controller-owned market whose slab key consents, or donate an existing fresh market authority; the permanent retirement marker makes each slab key single-generation under Meta governance. Squads can configure approved oracle modes, nonincreasing ordinary trade fees, nonincreasing stale-resolution and force-close deadlines after their first nonzero configuration, batch-safe backing-fee policies, bounded asset lifecycle, shutdown, resolution, and atomically reclaim terminal dust/rent from an empty slab. Every asset-scoped action is checked against Percolator's current market ID, while direct global resolution and its permissionless-resolution policy bind Percolator's next market ID, so approved terminal controls cannot cross a secondary activation or oracle restart. The proxy excludes unbounded `DrainOnly`; explicit shutdown starts Percolator's delayed permissionless force-close path. Every approved asset activation atomically clears the activated slot's two preserved backing-fee policies, so a retired predecessor profile cannot revive the pinned global batch gate. Terminal close reserves reclaimed rent in a permanent market-retirement PDA before forwarding the remainder. Its generic proxy cannot move insurance/backing, trade, rotate keys, or move portfolio collateral; fixed permissionless cleanup returns external insurance and backing only to their recorded providers, sends controller-owned protocol insurance through an empty one-shot transit to a clean Squads-vault-owned account, and deregisters only empty resolved portfolios. |
| `subledger/` | Owner-bound insurance/backing accounting. Genesis insurance pools bind market, Percolator program, COIN mint, policy, domain, deposit schedule, and bootstrap delay into the PDA. One base unit is one principal unit; priced shares keep losses scoped to each deposit's tenure while principal remains the vote/reward unit. After an exact total loss, a monotonic share generation restarts recapitalization without giving impaired shares a claim on new deposits. Every owner-signed complete exit binds exact principal and last-deposit slot, so an old authorization cannot retire a later insurance or segregated backing top-up. A principal-policy pool can hand live custody to TWAP with its protected floor. While that canonical current-layout pool owns all three asset-0 custody roles, anyone may clear only the two pinned batch-gating backing-fee policies to exact zero; the instruction accepts no fee, authority, amount, destination, or token account. A with-surplus pool cannot cross the TWAP boundary until every owner claim is gone; it may then reuse the same terminal path for later protocol fees or unowned rounding reserve. Its live TWAP full-exit entry point reuses the ordinary owner, pool, destination, loss, and share checks for atomic recovery. After recovery the pool can otherwise sign only fixed resolved cleanup. Once the bootstrap deadline has elapsed and the bound market is resolved and empty, anyone can retire an absent depositor's complete position only into a clean account owned by that depositor, preserving its remaining-principal and return-slot reward cap even if no proposal won. The same transaction makes its pool PDA retire that owner's exact live Genesis ballot before principal leaves. Its terminal read-only attestation proves when the pool has no owner claims. |
| `genesis-vote/` | Bootstrap decider. Each live principal base unit contributes one vote; principal is both the quorum denominator and proposal support. One voter backs one proposal. A current back commits to both the ballot nonce and exact live principal, so a withheld signature cannot count a later top-up; top-ups change no tally until a fresh re-back. Only a proposal with its complete declared entry shape and exactly 100% of the fixed supply allocated can become votable. New backing closes exactly when the configured bootstrap deadline makes the permissionless trigger live; retraction remains open so a vote lock cannot trap principal. The trigger seals the winner into `distribution`. An unsealed election gets exactly one deposit-window-long trigger phase; triggering rejects at the fallback boundary even before the first return, while retraction and principal recovery remain live. A terminal refund can remove only the exact recorded ballot when the configured Subledger pool PDA signs, making refunded votes worthless without changing a sealed allocation or selecting a former tie. A pre-flag executed proposal remains authoritative after upgrade even if a losing voter opened fallback first. Holds no funds. |
| `distribution/` | Claims from the fixed genesis COIN vault. A sealed proposal contains recipient/amount entries allocating at most the fixed supply. Each recipient authorizes its own claim; unclaimed or unallocated COIN is burned after the claim window. Never mints. |
| `residual-distributor/` | Reusable fixed or dynamic COIN reward epochs. It snapshots points from selected insurance/backing pools, realized residual flows, and cumulative funding paid (`long_paid + short_paid`, with no age multiplier), then pays only the position's bound recipient. Every portfolio-flow stake binds Percolator's market-owned monotonic portfolio ID, so crystallization and live-cap checks can use only the exact incarnation observed at registration; pre-ID stakes cannot resume counter accrual, but their already-frozen claims remain live-capped and claimable. Controller cleanup preserves terminal portfolio counters and their incarnation ID in a cumulative read-only PDA; each stake also binds the archive's allow-listed market, and the controller's immutable retirement marker prevents a reused slab key from admitting or influencing rewards. Witnesses must carry the exact Subledger-position discriminator or pinned Percolator portfolio provenance, and permissionless portfolio-flow claims reject delegated recipient accounts. It reads principal-bearing accounts but cannot debit them. |
| `twap-program/` | Post-genesis surplus auction and constrained asset-0 custodian. It can pull insurance above the monotonic floor only while asset 0 has no open position or unresolved loss state, run repeating uniform-price buybacks, cumulatively split bought COIN between burn and a bound reward vault, accept inbound insurance donations, hold or lower the initialized trade and bid fees while clearing predecessor backing-fee policies, and timelock-restart only the expected generation of an empty recovering asset 0. Exposed rounds still settle and advance without a fresh pull. Pool-bound recovery stays bound to the original genesis pool; a signing owner may use its fixed live full-exit proxy, whose value accounts are all revalidated by Subledger. After resolution, either that pool proves zero claims or a current-layout pool-less config proves it began empty; TWAP can then route the terminal protocol floor through an empty TWAP-owned transit only to a clean Squads-vault-owned account. A pool-less config can also sign only the controller's amountless, provider-bound asset-0 backing return. |
| `twap/` | Host-side auction simulation and Percolator wire helpers. It does not expose the obsolete raw Squads/operator rotation conveniences; deployed custody transitions live in `subledger` and `twap-program`. |
| `setup/` | Host helper for creating the fixed COIN supply and revoking mint authority. |

The workspace pins `percolator-prog` to commit
`da64be639168b496833dd4c701607a94fcb06b6c` and its engine layout dependency to
`143e68c4917ed0400a27b952f036a5677047cd84`.

That Percolator revision rejects every atomic batch while any backing utilization-fee policy is
active. Both governance wrappers therefore reject nonzero backing policies but preserve exact-zero
updates to clear predecessor state. The ordinary trade fee is selected when the market is initialized
and can only stay constant or decrease afterward: the pinned trade wire has a caller fee floor but no
user maximum, so an increase could reprice an already-signed trade. Safe increases require an upstream
max-fee field and a new pin. Backing fees can be re-enabled only after batch-safe accounting is merged on
top of this exact security line.

The auction's flat bid fee follows the same rule: book initialization selects its maximum and later
Squads actions may keep or lower it. The bid wire commits to amounts but has no maximum-fee field, so
allowing an increase could burn unrelated COIN from an already-signed bidder transaction.
Current cancellation wires commit to the reusable slot's placement clocks and both bid legs. The old
slot-only wire remains valid only for predecessor books that are exit-only under the upgraded binary.

## Genesis

1. **Create the market.** Anyone initializes a market with the governance-bound controller PDA as
   `marketauth`, but the fresh rent-funded slab key must sign once so another payer cannot consume it.
   Before user funding, Squads grants asset-0 custody to the canonical genesis pool;
   the only governance-held asset role is the oracle role.
2. **Deposit window.** Deposits are accepted only during
   `[bootstrap_start, bootstrap_start + deposit_window)`. The default window is about one week and
   cannot extend past bootstrap end. Topping up resets reward tenure but does not alter the Genesis
   rule that each live principal base unit contributes one vote.
3. **Bootstrap vote.** The default bootstrap delay is six 30-day months and is configurable in the
   pool/config PDA. A voter can vote immediately after depositing, and must retract before changing
   proposals or withdrawing. Quorum and the winning strict majority are computed from live,
   unwithdrawn principal. Current backs and retracts commit to the ballot's monotonic vote nonce, so
   an old signature cannot restore a withdrawn vote or remove a later one. The action-only predecessor
   wire remains usable for a nonce-zero predecessor or first ballot and for exit-only legacy
   configurations. A proposal is not votable until its declared entry capacity is full, so no creator
   action remains after voting.
   New backing closes at the bootstrap deadline; retraction remains available so the lock cannot
   trap principal while the permissionless trigger is pending or after it executes. Before an
   unsealed election can be unwound permissionlessly, every qualifying proposal has one configured
   deposit-window length in which any cranker can trigger it.
4. **Seal 100% of supply.** After the deadline, anyone can trigger a qualifying proposal. The
   winning allocation is immutable; recipients claim from the fixed vault and expired claims burn.
   A strict tie remains an unsealed COIN-distribution outcome. Triggering closes at the exact
   fallback-refund boundary, whether or not a return has executed, so a post-boundary owner exit
   cannot revive a former tie or expired quorum. The first fallback return also records that terminal
   state before changing any vote or quorum denominator. The tie cannot strand collateral or slab
   custody after the market reaches terminal state.
5. **Return deposits.** The intended genesis flow keeps custody in the owner-bound pool while
   initial risk takers redeem up to their loss-adjusted principal. If TWAP already holds custody,
   a signing owner can use the fixed atomic live path to return the roles and fully redeem their own
   remaining position without DAO cooperation. After resolution, returning the roles is
   permissionless while the bound market is empty and the pool still has owner principal. After the
   bootstrap deadline, any cranker can immediately retire an absent owner's full principal- or
   with-surplus-policy position into a clean token account owned by the depositor after a proposal
   seals. If none seals, permissionless returns open after the additional trigger phase described
   above. The first such fallback atomically closes the election, and every return subtracts its exact
   live ballot from proposal and global tallies. The instruction has no caller-selected amount or
   beneficiary, so one atom cannot veto terminal market closure or choose a distribution winner.
   Standalone with-surplus pools return their live share value pro rata and cannot enter TWAP
   custody while any owner claim remains. After every owner exits, the same handoff may route only
   later protocol fees or unowned rounding reserve through fixed terminal recovery.
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

TWAP rounds are externally cranked. A round pulls asset-0's live insurance surplus only when that
asset has no open position accounting or unresolved loss state, retains the configured insurance
share by ratcheting the floor upward, and buys COIN at one marginal
clearing price. Whole-atom fills reconcile the USD payout to the integer COIN actually bought; every
executed pair must still satisfy both the bidder's limit and the DAO reserve. Reconciliation releases
fully refunded nominal allocations to lower executable bids in the same round and maximizes COIN at
each unchanged USD payment even when no aggregate remainder remains. A bounded final-price replay also
restores skipped priority bids when they buy more COIN for the same spend while preserving the marginal
lot. Ordinary integer rounding remains in holding for a later round. A bid that cannot transact a
whole-atom pair cannot reserve nominal budget or set the marginal price. If every eligible bid is
integer-infeasible, the aged book settles for permissionless refunds without spending collateral.
Insurance deposited for other assets is never counted toward that surplus. Bought COIN can be split
between burn and a canonical dynamic reward vault. Fractional basis-point entitlement carries across
settled rounds, so repeated atom-sized fills cannot bias the configured cumulative split toward burn.
The auction and collateral-savings shares are likewise cumulative across permissionless rounds: TWAP
first carries the combined external share, then apportions only that bounded pull between the two routes.
Atom-sized cranks therefore cannot ratchet either configured share into insurance, and the combined pull
can never exceed current surplus. Each surplus pull atomically restores the two
asset-0 insurance domains to the 50/50 split of the newly ratcheted floor.
If exposure is present, the round still settles its existing book and opens the next round without
pulling insurance or changing the floor.
The collateral savings reserve must be distinct
from the auction holding; an aliased configuration fails atomically before either surplus pull, so
savings cannot enlarge the buy budget.

The full-chain test runs three 15-day rounds. Each round sends 50% of bought COIN to the reward
vault and burns 50%. At day 45 the accumulated reward vault distributes:

- 10% to selected insurance principal points;
- 10% to selected backing principal points;
- 80% to cumulative funding-payer points, summing long-paid and short-paid counters per portfolio.

Funding points have no age multiplier and receiver-side funding does not earn points. A portfolio
can earn from both its long-paid and short-paid totals. Insurance/backing reward points use live
base-unit principal times `floor(log2(tenure))`. Reward finalization and claims do not modify the
underlying principal, shares, or Percolator balances. A permissionless terminal genesis return
preserves only the remaining capital through its authenticated return slot, whether crystallized
before or after cleanup within the finalize window; ordinary owner exits still forfeit rewards.
Permissionless portfolio-flow claims pay only
to an initialized account owned by the bound recipient and reject any token delegate, so a cranker
cannot force rewards into an account it can spend. The same full-chain test then returns TWAP
custody and proves both the insurance depositor and the owner-bound collateral cohort used for
backing points can redeem without losing claimed COIN. That cohort is a standalone Subledger vault,
not a many-depositor adapter for Percolator backing buckets; the engine's external backing-provider
return lifecycle is covered separately.

Ordinary point growth closes at the reward epoch's inclusive end slot. During the finalize window,
capital owners may crystallize live or terminal insurance/backing at that fixed cutoff; every top-up
resets the Subledger position clock, so post-epoch capital earns zero tenure. Any cranker may only
reduce a trader stake whose previously crystallized loss has since been spent; the shared claim cap
makes this refresh monotonic, so post-epoch flow cannot mint points or dilute the frozen denominator.

Each reward epoch binds its authority, COIN mint, schedule, percentages, canonical vault, and up to
six selected market/pool scopes. A maximal six-scope initialization fits a one-member-signed Squads
transaction under the network packet limit. Insurance and backing allocations must name distinct
Subledger pools in both genesis and continuous epochs, so one position cannot enter both capital cohorts.
The DAO may configure future epochs but cannot mutate a live epoch,
redirect a user's claim, or sweep principal. After a sink epoch freezes, later rounds burn that stale
sink share rather than writing outside the snapshot. A closed or otherwise invalid exact-key COIN sink
also falls back to burn instead of stalling settlement. The configured COIN sink must remain external
to the book's COIN escrow, collateral settlement escrow, and holding account, including when COIN is
also the collateral mint.

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
and bounded asset lifecycle, approved oracle configuration, bounded fee policy, resolution, and
empty-slab cleanup. `DrainOnly` is excluded because it blocks replacement liquidity without starting
the force-close clock; governance uses explicit shutdown so every position has a bounded public exit.
Raw `CloseSlab` is not proxied: the dedicated cleanup forwards its mandatory controller
destinations atomically. The proxy excludes deposits, withdrawals, swaps, portfolio operations,
authority rotation, and backing-bucket movement. External backing providers retain their own
asset-local withdrawal path; governance only sets the fee split that sends the configured share
into insurance. Secondary activation may independently select backing and oracle providers, but its
insurance authority and operator remain the constrained controller so governance cannot install a
raw withdrawal key.

Asset-scoped proxy calls append one final read-only witness PDA derived from
`("asset-generation", market, asset_index, market_id)`. This applies to activation and other
lifecycle changes, restart, hybrid/EWMA/authenticated oracle configuration, and both backing-fee
domains. The controller verifies the current Percolator market ID and removes the witness before
CPI; hybrid feed accounts remain immediately before it. An unconfigured secondary slot uses market
ID `0`. For queued-action compatibility, the atomically created asset-0 generation `1` alone accepts
its predecessor wire without a witness, and only while that generation remains current. Every
secondary and replacement generation is strict. Governance must rebuild a queued asset action after
its target slot changes generation.

Market-wide `ResolveMarket` calls append a separate read-only witness derived from
`("market-generation", market, next_market_id)`. Percolator advances `next_market_id` whenever any
asset is created or restarted, so an approved global resolution cannot survive into a replacement
asset generation. The controller removes this witness before CPI and stores no generation state.

Portfolio owners normally close their own empty accounts. Once a market is resolved, an absent owner
cannot hold `materialized_portfolio_count` above zero forever: any cranker can invoke the controller's
fixed portfolio cleanup. It signs only pinned Percolator `ClosePortfolio`; Percolator rejects live or
nonempty portfolios and sends the closed account's lamports only into the bound market slab. If a
reward-relevant counter is nonzero, the legacy cleanup shape rejects and the extended shape first
archives the authenticated counters; both CPIs are atomic. Reward registration and crystallization
sum that canonical archive with the current live generation. Registration binds the live portfolio
to its real Percolator market and rejects a nonzero immutable maintenance fee; capital cohorts are
unaffected. Terminal `CloseSlab` also creates a permanent marker keyed by Percolator program and slab;
new portfolio-flow stakes reject that retired key, and existing stakes ignore any public reinitialization
  under it. A pre-archive empty-account fallback remains claim-only for frozen LP received-flow,
  which is monotonic even if a third party donates lamports to the closed address. Trader loss points
  require either the live portfolio or the controller archive because later public trades can raise
  their spent counter and lower the claim cap; an owner who directly closes first forfeits those
  unverifiable points. The wrapper accepts no
amount, token account, or destination and exposes no generic portfolio authority. Telemetry-bearing
cleanup requires the exact retirement-marker PDA: before slab retirement it archives the original
generation, and after retirement the controller refuses to admit or act on later generations.

An absent insurance authority or backing provider cannot block secondary-asset retirement with one
remaining atom. After Squads shuts the asset down and Percolator's delay and empty-state checks pass,
anyone can return its complete asset-local insurance and each backing domain through the controller.
The controller derives the insurance amount from the pinned slab and accepts no caller-selected
amount. Each destination must be a clean token account owned solely by the recorded provider, not a
DAO-selected beneficiary; this lets a cranker bypass a permanently frozen ATA without redirecting
funds. The temporary receiving account is likewise replaceable but must be a clean same-mint account
owned solely by the controller PDA. Backing earnings are paid first and the controller forwards
exactly the value attributed to that provider. It closes the temporary controller account only when no unrelated balance
remains. External-provider value already held there stays isolated from the exact provider return.
Fee insurance whose recorded authority and operator are both the controller instead uses an empty
one-shot transit and a clean Squads-vault-owned destination. Its fixed cleanup first rotates only the local operator to an
asset-scoped instruction-only PDA, which makes Percolator itself enforce the delayed shutdown and
empty-asset checks. Only after Percolator withdraws the complete balance does the same atomic call
restore the controller operator, so a later oracle restart does not inherit the one-shot role. The
transit starts empty, forwards the exact amount to governance, and closes atomically; a caller cannot
redirect or fragment it, and a frozen canonical account is never required. While live, no return can
bypass Percolator's delayed secondary-asset shutdown override.

Global stale resolution is permissionless, so a cranker can resolve before those shutdown returns
run. The resolved companions require the whole market to be resolved and empty, derive every amount
from the slab, and use the controller's existing secondary `asset_admin` role to rotate only the
relevant insurance or backing role. They then return all value to a clean account owned solely by
the outgoing recorded provider, but only up to the exact amount attributed by that cleanup. The caller and DAO
still choose neither an amount nor a recipient, and any failed rotation, withdrawal, forwarding, or
close rolls the entire operation back.

If the slab instead records the controller itself as insurance authority, including for asset 0,
the resolved companion withdraws only the exact asset-local amount through an empty controller-owned
transit to a clean account owned by the bound Squads vault, then closes the transit. It cannot select
another beneficiary or leave a persistent controller balance.

The same terminal custody applies to TWAP-retained insurance after all genesis owners exit. The
subledger program first attests that its bound principal pool has no outstanding principal or
shares; only then can a permissionless TWAP crank route the exact resolved asset-0 remainder through
an empty clean TWAP-owned transit into a clean account owned by the bound Squads vault. Replaceable
accounts prevent a permanently frozen canonical ATA from blocking cleanup, while exact ownership and
the empty source prevent redirection or an unrelated TWAP balance sweep. Once
the slab's asset-0 insurance is zero, a public role-only return to the same pool is safe and lets its
existing fixed wrapper complete asset-0 backing cleanup without a surviving DAO.

Asset 0 has no per-asset shutdown override. After whole-market resolution, its separate fixed path
reads both domains' complete principal and earnings from the pinned slab, atomically transfers the
backing role to the controller, and returns the full value to a clean account owned solely by the
outgoing provider. Before genesis custody moves, the controller is the constrained asset admin; after TWAP,
custody first returns to the canonical pool, whose only backing action is invoking this same fixed
cleanup. A failed domain CPI rolls back every earlier transfer and the authority change.

Permissionless market donation transfers lifecycle control, not funded creator capital. If the
outgoing raw market authority still owns nonzero asset-0 insurance, donation rejects atomically; the
creator withdraws first, then the unchanged permissionless handoff migrates both empty insurance
roles to the controller. The recorded backing provider remains unchanged. A handoff that preserves
a nonzero backing bucket is rejected unless `asset_admin` migrates to the controller. The only
non-migrating exception requires the existing handoff to include canonical current-layout Subledger or TWAP state
bound to this exact market and Percolator program; both insurance roles must name the same constrained
PDA. An arbitrary delegated admin is rejected even while empty because it could fund after handoff.
The insurance authority and operator must also match before handoff and belong to the outgoing
authority, controller, or attested Subledger/TWAP custodian. An arbitrary equal-role third party could
otherwise skim fee insurance generated by later public trades despite risking no insurance capital.
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
use permissionless controller initialization followed by governance-approved asset activation. Both
insurance roles stay on the constrained controller, while external backing and oracle providers remain
independently selectable. This prevents an unfunded raw key from collecting user-paid trade fees.
The same constrained proxy can restart an empty recovering asset through Percolator's value-neutral
restart instruction only while both insurance roles remain on the controller; it cannot choose a
recipient or move insurance/backing while doing so. A deployed predecessor asset that still names an
external insurance key can finish recovery and retire, but cannot reopen and credit that zero-capital
key with fees from a new lifecycle.
Here, empty means every Percolator position, funding, loss, spent-budget, backing, and reservation
ledger is zero, not only zero OI. A previously traded slot with residual K/F accumulators cannot use
restart and must complete terminal recovery before governance initializes a fresh controller market.

At genesis-pool grant, the controller moves the oracle role to Squads and then atomically moves both
insurance roles and `asset_admin` to the pool. Squads may self-rotate the oracle role to an approved
builder, but it cannot use that role to move insurance or backing. Post-genesis, TWAP receives both
insurance roles and `asset_admin`; its exposed Percolator CPIs are fixed-purpose and accept no
arbitrary withdrawal destination. Governance therefore has no arbitrary resolved-mode insurance
withdrawal; external funds use the provider-bound fixed cleanup, while controller-owned protocol
insurance can move only through an empty one-shot transit to a clean Squads-vault-owned account.
TWAP's fixed restart wire also commits to asset 0's current monotonic Percolator market ID. A queued
restart rejects after any other action replaces that generation; the comparison is read-only and
does not add an authority, account, recipient, or collateral path.

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
  --test chain e2e_full_genesis_to_buy_burn \
  -- --exact --nocapture
```

LiteSVM loads the Cargo-pinned Percolator SBF, real Squads v4 fixture, and locally built program
binaries. The end-to-end assertions exercise real CPIs rather than mocks. Oracle pushes and other
permissionless cranks are triggered externally in the test, matching the intended deployment.

## License

[Apache License 2.0](LICENSE). Provided as is for educational use.
