#!/usr/bin/env bash
# Silent Mesh — Phase 2 exit-criterion dry run (roadmap "Phase 2 → Exit").
#
# Drives the whole exit criterion through buzz-cli against a live relay and
# prints a PASS/FAIL matrix. Every check asserts an observable outcome, so a
# regression in any Phase 2 slice fails the run.
#
# Prereqs:
#   - Postgres + Redis + MinIO (the repo's dev compose stack)
#   - A relay started against them (see RELAY_URL below), with:
#       BUZZ_WORKSPACE_CHANNEL_GATE=true   (D42 authority is under test)
#       RELAY_OWNER_PUBKEY=$OWNER_PK       (the workspace Owner)
#       BUZZ_RELAY_PRIVATE_KEY=<hex>       (buzz-admin needs a stable relay
#                                           signer to publish roster events)
#       BUZZ_USAGE_METRICS_INTERVAL_SECS=15 (optional, recommended: the
#                                           overdue sweep rides this tick,
#                                           default 300s — the run waits
#                                           ~5 min longer without it)
#     Point it at a scratch port/host, NOT a production relay: the run
#     creates channels, threads, and members in that community.
#   - `buzz` built: cargo build -p buzz-cli
#
# Usage:
#   IDS_ENV=/path/to/ids.env RELAY_URL=ws://localhost:3999 \
#     docs/silent-mesh/phase2-exit-dryrun.sh
#
# IDS_ENV supplies OWNER_SK/OWNER_PK (the relay's configured owner — it must
# match RELAY_OWNER_PUBKEY). Every other identity is minted fresh per run via
# `buzz-admin generate-key`, so repeated runs never collide on once-per-member
# state such as the personal channel.

set -uo pipefail

BUZZ=${BUZZ:-./target/debug/buzz}
BUZZ_ADMIN=${BUZZ_ADMIN:-./target/debug/buzz-admin}
export BUZZ_RELAY_URL=${RELAY_URL:-ws://localhost:3999}
# shellcheck disable=SC1090
source "${IDS_ENV:?set IDS_ENV to the identities file}"

# Fresh participants per run — the owner is fixed (relay config), the rest
# are minted so once-per-member state (personal channels) starts clean.
mint() { # mint <VARPREFIX>
    local out sk pk
    out=$("$BUZZ_ADMIN" generate-key 2>/dev/null)
    pk=$(awk '/Public key/ {print $3}' <<<"$out")
    sk=$(awk '/Secret key/ {print $3}' <<<"$out")
    printf -v "${1}_SK" '%s' "$sk"; printf -v "${1}_PK" '%s' "$pk"
}
for who in WSADMIN CHADMIN MEMBER MEMBER2 AGENT; do mint "$who"; done

PASS=0
FAIL=0
RESULTS=()

# as <secret-key> <args...> — run buzz as a given identity.
as() { local sk=$1; shift; BUZZ_PRIVATE_KEY="$sk" "$BUZZ" "$@" 2>&1; }

check() { # check <name> <condition-description> <0|1 result>
    local name=$1 detail=$2 ok=$3
    if [ "$ok" = "0" ]; then
        PASS=$((PASS + 1)); RESULTS+=("PASS  $name")
    else
        FAIL=$((FAIL + 1)); RESULTS+=("FAIL  $name — $detail")
    fi
}

expect_ok() { # expect_ok <name> <output>
    local name=$1 out=$2
    if grep -q '"accepted":true' <<<"$out"; then check "$name" "" 0
    else check "$name" "expected acceptance, got: ${out:0:200}" 1; fi
}

expect_refused() { # expect_refused <name> <needle> <output>
    local name=$1 needle=$2 out=$3
    if grep -qi "$needle" <<<"$out"; then check "$name" "" 0
    else check "$name" "expected refusal matching '$needle', got: ${out:0:200}" 1; fi
}

jqv() { python3 -c "import sys,json;d=json.load(sys.stdin);print(d$1 if d else '')" 2>/dev/null; }

echo "== Phase 2 exit-criterion dry run =============================="
echo "relay: $BUZZ_RELAY_URL"

# ── Setup: workspace roles, team channel, membership ──────────────────────
# Workspace roles live in `relay_members` (D42 reuses the NIP-43 roster).
# The operator CLI is the management surface; every member of this run needs
# a roster row because the relay enforces membership.
# `buzz-admin` publishes a roster event, so it needs the relay's signing
# key. Failing here silently would cascade into every later check, so this
# aborts loudly instead.
: "${BUZZ_RELAY_PRIVATE_KEY:?set BUZZ_RELAY_PRIVATE_KEY (same value the relay runs with) so buzz-admin can seed the roster}"
seed_member() { # seed_member <pubkey> <role>
    local out
    if ! out=$("$BUZZ_ADMIN" add-member --pubkey "$1" --role "$2" 2>&1); then
        echo "SETUP FAILED: buzz-admin add-member $2 -> $out" >&2
        exit 2
    fi
}
seed_member "$WSADMIN_PK" admin
for pk in "$CHADMIN_PK" "$MEMBER_PK" "$MEMBER2_PK" "$AGENT_PK"; do
    seed_member "$pk" member
done

# D42: a Workspace Admin may create a team channel (gate is ON).
OUT=$(as "$WSADMIN_SK" channels create --name product --type stream --visibility private)
expect_ok "workspace-admin creates a team channel (D42)" "$OUT"
TEAM=$(jqv "['channel_id']" <<<"$OUT")
if [ -z "$TEAM" ]; then
    echo "SETUP FAILED: no team channel - every later check would cascade. Response: $OUT" >&2
    printf '%s\n' "${RESULTS[@]}"
    exit 2
fi

# A plain member may NOT (the gate is on).
OUT=$(as "$MEMBER_SK" channels create --name sneaky --type stream --visibility private)
expect_refused "plain member's channel creation refused (gate)" "workspace owner/admin" "$OUT"

# The Workspace Admin appoints the Channel Admin and adds members.
as "$WSADMIN_SK" channels add-member --channel "$TEAM" --pubkey "$CHADMIN_PK" --role admin >/dev/null
as "$WSADMIN_SK" channels add-member --channel "$TEAM" --pubkey "$MEMBER_PK" --role member >/dev/null
as "$WSADMIN_SK" channels add-member --channel "$TEAM" --pubkey "$MEMBER2_PK" --role member >/dev/null
as "$WSADMIN_SK" channels add-member --channel "$TEAM" --pubkey "$AGENT_PK" --role bot >/dev/null
OUT=$(as "$WSADMIN_SK" channels members --channel "$TEAM")
if grep -q "$CHADMIN_PK" <<<"$OUT" && grep -q "$AGENT_PK" <<<"$OUT"; then
    check "workspace admin appoints Channel Admin + roster" "" 0
else
    check "workspace admin appoints Channel Admin + roster" "roster incomplete: ${OUT:0:200}" 1
fi

# ── Task thread: goal, deadline, DRI = the agent (D40) ────────────────────
DEADLINE=$(( $(date +%s) + 86400 ))
OUT=$(as "$MEMBER_SK" threads open --channel "$TEAM" --goal "fix the parser" \
        --deadline "$DEADLINE" --dri "$AGENT_PK")
expect_ok "member launches a task thread (goal/deadline/DRI)" "$OUT"
THREAD=$(jqv "['thread_id']" <<<"$OUT")

# ── Agent work: checkpoints (the worktree trail) ──────────────────────────
C1=$(printf 'aa%.0s' {1..20})   # 40-hex stand-ins for pushed commits
C2=$(printf 'bb%.0s' {1..20})
OUT=$(as "$AGENT_SK" threads checkpoint --channel "$TEAM" --thread "$THREAD" \
        --commit "$C1" --branch "thread/parser" --turn 1 --note "turn 1")
expect_ok "agent checkpoint lands in the stream (47010)" "$OUT"
as "$AGENT_SK" threads checkpoint --channel "$TEAM" --thread "$THREAD" \
    --commit "$C2" --branch "thread/parser" --turn 2 --note "turn 2" >/dev/null

# ── Agent recommends done — inert until a human confirms (D41) ────────────
OUT=$(as "$AGENT_SK" threads recommend --channel "$TEAM" --thread "$THREAD" \
        --note "parser fixed; ready to close")
expect_ok "agent posts a done-recommendation (47003)" "$OUT"
STATUS=$(as "$MEMBER_SK" threads show --channel "$TEAM" --thread "$THREAD" | jqv "['status']")
check "recommendation changes nothing (still open)" "status=$STATUS" \
      "$([ "$STATUS" = "open" ] && echo 0 || echo 1)"

# An agent cannot transition state at all.
OUT=$(as "$AGENT_SK" threads state --channel "$TEAM" --thread "$THREAD" --to ready)
expect_refused "agent state transition refused (agents recommend)" "D41" "$OUT"

# ── Member authority: metadata edit yes, close no (D41) ───────────────────
NEW_DEADLINE=$(( $(date +%s) + 172800 ))
OUT=$(as "$MEMBER_SK" threads set --channel "$TEAM" --thread "$THREAD" --deadline "$NEW_DEADLINE")
expect_ok "member's metadata edit (new deadline) succeeds" "$OUT"

as "$MEMBER_SK" threads state --channel "$TEAM" --thread "$THREAD" --to ready >/dev/null
OUT=$(as "$MEMBER_SK" threads state --channel "$TEAM" --thread "$THREAD" --to closed)
expect_refused "plain member's close attempt refused" "D41" "$OUT"

# ── Fork at an earlier checkpoint, drive a variation (D27) ────────────────
OUT=$(as "$MEMBER2_SK" threads fork --channel "$TEAM" --thread "$THREAD" \
        --goal "try approach B" --commit "$C1")
expect_ok "second member forks at an earlier checkpoint" "$OUT"
FORK=$(jqv "['thread_id']" <<<"$OUT")
FORKED_FROM=$(as "$MEMBER2_SK" threads show --channel "$TEAM" --thread "$FORK" | jqv "['forked_from']")
check "fork carries provenance to the parent" "forked_from=$FORKED_FROM" \
      "$([ "$FORKED_FROM" = "$THREAD" ] && echo 0 || echo 1)"

# A fork point that was never checkpointed is refused.
BOGUS=$(printf 'cc%.0s' {1..20})
OUT=$(as "$MEMBER2_SK" threads fork --channel "$TEAM" --thread "$THREAD" \
        --goal "bogus" --commit "$BOGUS")
expect_refused "fork at an unrecorded checkpoint refused" "recorded checkpoint" "$OUT"

# ── Channel Admin closes the winner: canonicalize + archive siblings ──────
as "$MEMBER_SK" threads state --channel "$TEAM" --thread "$FORK" --to ready >/dev/null
OUT=$(as "$CHADMIN_SK" threads state --channel "$TEAM" --thread "$FORK" --to closed \
        --canonicalize --archive-siblings)
expect_ok "Channel Admin closes with canonicalization + sibling archiving" "$OUT"
if grep -q 'archived_siblings.\{0,2\}:1' <<<"$OUT"; then
    check "winner's close archived the losing sibling (D28)" "" 0
else
    check "winner's close archived the losing sibling (D28)" "response: ${OUT:0:200}" 1
fi
LOSER_STATUS=$(as "$MEMBER_SK" threads show --channel "$TEAM" --thread "$THREAD" | jqv "['status']")
check "losing thread folds to archived" "status=$LOSER_STATUS" \
      "$([ "$LOSER_STATUS" = "archived" ] && echo 0 || echo 1)"

# The canonicalization outcome notice (47012) reaches the stream and is
# visible from the CLI (threads show → notices).
#
# SCOPE: this run drives the event/authority plane with synthetic commit
# ids, so the honest outcome here is `commit_missing` — the checkpoint
# names a commit nobody pushed. The merged-into-canon/ path is covered by
# the S3-gated probe
# (`BUZZ_GIT_S3_PROBE=1 cargo test -p buzz-relay --lib canonicalize::s3_probe_tests -- --ignored`),
# which seeds real git objects. What matters here is that the job ran,
# reached a terminal outcome, and reported it into the channel.
sleep 3
CANON=$(as "$CHADMIN_SK" threads show --channel "$TEAM" --thread "$FORK")
OUTCOME=$(grep -o '"outcome": *"[a-z_:]*"' <<<"$CANON" | head -1 | sed 's/.*"\([a-z_:]*\)"$/\1/')
if grep -q 'canonicalized' <<<"$CANON"; then
    check "canonicalization runs and reports its outcome (47012: ${OUTCOME:-?})" "" 0
else
    check "canonicalization runs and reports its outcome (47012)" "no 47012 in notices: ${CANON:0:200}" 1
fi
# The losing sibling carries its own archive notice (47013).
SIB=$(as "$CHADMIN_SK" threads show --channel "$TEAM" --thread "$THREAD")
if grep -q "sibling_archived" <<<"$SIB"; then
    check "losing sibling carries its archive notice (47013)" "" 0
else
    check "losing sibling carries its archive notice (47013)" "no 47013 in notices: ${SIB:0:200}" 1
fi

# ── Owner reopen is owner-only (D41) ──────────────────────────────────────
as "$CHADMIN_SK" threads state --channel "$TEAM" --thread "$FORK" --to archived >/dev/null
OUT=$(as "$CHADMIN_SK" threads state --channel "$TEAM" --thread "$FORK" --to open)
expect_refused "Channel Admin's reopen-from-archived refused" "D41" "$OUT"
OUT=$(as "$OWNER_SK" threads state --channel "$TEAM" --thread "$FORK" --to open)
expect_ok "workspace Owner reopens from archived" "$OUT"

# ── Personal channel + gated promotion (D29/D30) ──────────────────────────
OUT=$(as "$MEMBER_SK" channels create-personal --name my-space)
expect_ok "member creates their personal channel (D29)" "$OUT"
PERSONAL=$(jqv "['channel_id']" <<<"$OUT")

OUT=$(as "$MEMBER_SK" channels create-personal --name second-space)
expect_refused "second personal channel refused (one per member)" "already has a personal channel" "$OUT"

# The private-channel membership check fires first for a true outsider;
# the personal-channel guard is what stops an *existing* member of the
# workspace from being added by the owner as a non-agent.
OUT=$(as "$MEMBER2_SK" channels add-member --channel "$PERSONAL" --pubkey "$MEMBER2_PK" --role member)
expect_refused "outsider cannot join a personal channel" "not a channel member\|authoriz\|personal" "$OUT"
OUT=$(as "$MEMBER_SK" channels add-member --channel "$PERSONAL" --pubkey "$MEMBER2_PK" --role member)
expect_refused "personal owner cannot add a non-agent member (D29)" "own agents\|personal" "$OUT"

OUT=$(as "$MEMBER_SK" threads open --channel "$PERSONAL" --goal "private exploration")
expect_ok "member opens a thread in their personal channel" "$OUT"
PTHREAD=$(jqv "['thread_id']" <<<"$OUT")
P1=$(printf 'dd%.0s' {1..20})
as "$MEMBER_SK" threads checkpoint --channel "$PERSONAL" --thread "$PTHREAD" --commit "$P1" >/dev/null

# The Privacy Gate refuses a credential-shaped summary before any git work.
OUT=$(as "$MEMBER_SK" threads promote --from "$PERSONAL" --thread "$PTHREAD" --to "$TEAM" \
        --summary "creds AKIAIOSFODNN7EXAMPLE")
expect_refused "privacy gate refuses a secret-bearing summary (D30)" "privacy gate" "$OUT"

# Someone else cannot promote out of another member's personal channel.
OUT=$(as "$MEMBER2_SK" threads promote --from "$PERSONAL" --thread "$PTHREAD" --to "$TEAM" \
        --summary "not mine")
expect_refused "non-owner cannot promote from a personal channel" "personal-channel owner" "$OUT"

# ── Overdue deadline tags the DRI (D40) ───────────────────────────────────
PAST=$(( $(date +%s) - 3600 ))
OUT=$(as "$MEMBER_SK" threads open --channel "$TEAM" --goal "overdue task" \
        --deadline "$PAST" --dri "$AGENT_PK")
expect_ok "thread opened with a past deadline" "$OUT"
OVERDUE_THREAD=$(jqv "['thread_id']" <<<"$OUT")
[ -z "$OVERDUE_THREAD" ] && OVERDUE_POLLS=0 || OVERDUE_POLLS=60
# The overdue sweep rides the leader-elected metrics tick (minutes, not
# seconds) — poll patiently rather than assuming it is instant.
OVERDUE=""
for _ in $(seq 1 "$OVERDUE_POLLS"); do
    sleep 10
    OVERDUE=$(as "$CHADMIN_SK" threads show --channel "$TEAM" --thread "$OVERDUE_THREAD")
    grep -q '"overdue"' <<<"$OVERDUE" && break
done
if grep -q '"overdue"' <<<"$OVERDUE" && grep -q "$AGENT_PK" <<<"$OVERDUE"; then
    check "overdue sweep tags the DRI in the stream (47011)" "" 0
else
    check "overdue sweep tags the DRI in the stream (47011)" "no DRI-tagged 47011: ${OVERDUE:0:200}" 1
fi

# ── A non-member sees nothing ─────────────────────────────────────────────
OUT=$(as "$AGENT_SK" threads list --channel "$PERSONAL" 2>&1)
if grep -q "$PTHREAD" <<<"$OUT"; then
    check "non-member sees nothing in a private channel" "leaked: ${OUT:0:160}" 1
else
    check "non-member sees nothing in a private channel" "" 0
fi

# ── Report ────────────────────────────────────────────────────────────────
echo
printf '%s\n' "${RESULTS[@]}"
echo
echo "== $PASS passed, $FAIL failed ================================="
[ "$FAIL" -eq 0 ]
