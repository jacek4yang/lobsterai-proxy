# Daily check-in and credits

LobsterAI grants +100 credits per account per day through a client-activity
flow. This proxy implements it natively (the previous standalone
`auto-sign.py` script was folded into the server and deleted).

## Upstream flow

```
GET /api/client-activities/slot?placement=desktop_sidebar&clientVersion=2026.9.4&containerApiVersion=2&platform=win32
    -> data.slotState == "available"
    -> data.activity.{activityCode, configRevision}

GET /api/client-activities/{code}/context?configRevision={rev}
    -> data.state.claimedToday == false
    -> data.actions includes "check_in"

POST /api/client-activities/{code}/actions/check_in
    body: {"configRevision": rev, "idempotencyKey": "<uuid4>", "payload": {}}
    -> data.result.creditsGranted (field name varies; parsed defensively)
```

## Load-bearing details

- **clientVersion gate**: the slot endpoint is queried with
  `User-Agent: LobsterAI/2026.9.4`. Any lower version yields
  `slotState=empty` for every account — the value must track the official
  client's minimum. It lives in `lobsterai/credential.rs::CLIENT_VERSION`.
- **Idempotency**: every check-in POST carries a fresh uuid4
  `idempotencyKey`. Repeating the flow after a network failure cannot
  double-claim; the upstream also reports `claimedToday` in the context, so
  re-running the loop is always safe.
- **Schedule**: the housekeeping loop runs check-in for every account
  hourly by default (`[checkin] interval_secs`), starting ~30 s after boot.
  An account whose token was rejected is refreshed once (single-flight) and
  the check-in retried in the same pass.

## Credits and pool selection

After each check-in pass the proxy queries
`GET /api/user/profile-summary` (`totalCreditsRemaining`, includes campaign
credits) and stores the learned value in the pool. Account selection for
requests prefers the healthy account with the highest known remaining
credits; accounts with unknown credits rank last. When an account runs out
of credits mid-request, it receives a long cooldown (12 h by default,
`hard_credit_cooldown_secs`) so traffic rotates to accounts that still have
quota — including accounts topped up by the next check-in.

## Status

- `GET /admin/status` shows per-account health, learned credits, and
  cooldowns (safe names only).
- `lobsterai-proxy status` prints the same summary from the CLI.
