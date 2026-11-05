# NetFlux UI and backend maintenance

The dashboard is built with egui/eframe. This maintenance pass also fixes
correctness issues found in traffic shaping and process attribution.

## UI

- Dark dashboard palette, traffic summary cards, live graphs and process history.
- Search by executable name or full path.
- Filters for all, active, limited and blocked processes.
- Sortable process table and process detail actions.
- Per-process download/upload limits, block/unblock and rule removal.
- Expired process rows release their cached process-icon textures.

## Backend fixes

- Queued packets respond to rule changes: blocked/zero-limit queues are
  discarded, while queues whose direction becomes unlimited are released.
- Oversized packets are charged at their full packet size by the token bucket.
- Traffic statistics include only packets successfully reinjected.
- PID mappings are revalidated regularly to reduce incorrect attribution after
  Windows reuses a process ID.
- Complete socket-table refreshes replace stale flow mappings and preserve
  flow events received during the refresh.
- Port-owner fallback includes the local IP and checks wildcard-bound sockets.
- Inactive process counters expire after two minutes without live socket or
  packet activity.
- IPv6 extension headers are traversed before extracting TCP/UDP ports.

## Validation still required

CI verifies formatting, linting, tests and Windows compilation. Runtime checks
must still be performed on Windows with Administrator privileges: verify live
rates, independent upload/download limits, immediate block/unblock, rule
removal while packets are queued, restart persistence, VPN/proxy behavior and
Stremio streaming under a deliberately low limit.
