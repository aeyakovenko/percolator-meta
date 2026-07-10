# Percolator Meta Architecture Contract

The executable design is described in [README.md](README.md). This file records the security
contract that changes to the programs must preserve.

## Genesis

- COIN has a fixed supply, no mint authority, and no freeze authority.
- The default deposit window is about one week. The bootstrap delay is configurable and defaults
  to six 30-day months; both values are bound into the genesis pool/config PDAs.
- One base unit is one unit of live principal. Vote support additionally uses the configured
  log-time multiplier. Withdrawn principal no longer contributes to quorum.
- A winning proposal allocates at most the complete fixed supply. Claims are recipient-bound and
  expired/unallocated COIN is burned.
- Genesis insurance is share accounted and owner bound. No governance instruction can redirect an
  owner's exit.

## Authority Separation

- `market-controller` permanently holds `marketauth` and exposes only the exact pinned Percolator
  lifecycle/oracle/fee allow-list. Live deposits, withdrawals, backing movement, portfolio calls,
  swaps, and authority mutation are denied.
- Raw `CloseSlab` is excluded from the generic proxy. The fixed terminal cleanup is allowed only
  after Percolator proves every attributed balance and portfolio is zero, and atomically forwards
  its mandatory controller-owned token and lamport destinations to governance.
- Asset-0 custody moves only
  `market-controller -> genesis pool -> TWAP PDA -> original genesis pool`.
- Squads authorizes transitions through a minimum one-week timelock but never receives a funded
  market's insurance authority, insurance operator, backing authority, or `asset_admin`.
- The pool-to-TWAP transition atomically binds the source pool and raises the TWAP floor to at
  least that pool's current outstanding principal. The floor can never decrease or re-arm its
  unset sentinel.
- External backing providers retain their asset-local withdrawal authority. Governance controls
  lifecycle and fee policy, not provider principal.

## Rewards

- Reward programs read principal and cumulative engine counters; they never debit insurance,
  backing, or portfolio collateral.
- Dynamic reward epochs bind authority, COIN mint, schedule, market/pool scopes, percentages, and
  the canonical COIN vault at initialization.
- Funding-payer points equal cumulative long-paid plus short-paid atoms for the portfolio, with no
  age multiplier. Funding received does not earn payer points.
- A position may claim each distinct configured reward family once. Claims are paid only to the
  position's bound recipient.
- TWAP can route bought COIN to burn or a bound reward vault. Its Percolator withdrawal is limited
  to insurance above the monotonic floor; inbound donations and market fee updates accept no
  arbitrary withdrawal destination.

## Verification

Security changes require a red-then-green LiteSVM probe against the Cargo-pinned Percolator SBF and
real Squads fixture. The full chain test must continue to cover permissionless market creation,
genesis, long- and short-side funding, custody handoff, repeating TWAP rounds, and final reward
claims without modifying user principal.
