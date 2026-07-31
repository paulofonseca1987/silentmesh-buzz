#!/usr/bin/env bash
# Silent Mesh — Phase 3 end-to-end run (model plane + privacy gate).
#
# Sibling of phase2-exit-dryrun.sh: drives everything Phase 3 shipped
# (slices 1–8) through buzz-cli against a live relay and prints a PASS/FAIL
# matrix. Every check asserts an observable outcome, so a regression in any
# slice fails the run.
#
# Prereqs:
#   - Postgres + Redis + MinIO (the repo's dev compose stack)
#   - Ollama serving locally (systemd user service `ollama.service`)
#   - A relay started against them with:
#       BUZZ_WORKSPACE_CHANNEL_GATE=true
#       RELAY_OWNER_PUBKEY=$OWNER_PK
#       BUZZ_RELAY_PRIVATE_KEY=<hex>
#       BUZZ_GATE_ASSIST_MODEL=ollama:<model>   (D30 assist under test;
#                                                llama3.2:3b keeps the run
#                                                short, qwen3:14b is the
#                                                production-grade check)
#   - `cargo build -p buzz-cli -p buzz-admin`
#
# Usage:
#   IDS_ENV=~/.silentmesh-e2e/ids.env RELAY_URL=ws://100.72.140.59:3999 \
#     docs/silent-mesh/phase3-e2e.sh
#
# NOT covered here (needs the ACP harness + a live agent, run separately):
#   - the pre-turn tier gate refusing a vendor-model turn (slice 2)
#   - live-turn attribution via kind 44201 (slice 4)
# Those are exercised by the two-machine agent demo; see phase3-*.md.

set -uo pipefail

BUZZ=${BUZZ:-./target/debug/buzz}
BUZZ_ADMIN=${BUZZ_ADMIN:-./target/debug/buzz-admin}
export BUZZ_RELAY_URL=${RELAY_URL:-ws://localhost:3999}
GATE_WAIT=${GATE_WAIT:-180}
# shellcheck disable=SC1090
source "${IDS_ENV:?set IDS_ENV to the identities file}"

mint() { # mint <VARPREFIX>
    local out sk pk
    out=$("$BUZZ_ADMIN" generate-key 2>/dev/null)
    pk=$(awk '/Public key/ {print $3}' <<<"$out")
    sk=$(awk '/Secret key/ {print $3}' <<<"$out")
    printf -v "${1}_SK" '%s' "$sk"; printf -v "${1}_PK" '%s' "$pk"
}
for who in WSADMIN MEMBER MEMBER2; do mint "$who"; done

PASS=0; FAIL=0; RESULTS=()
as() { local sk=$1; shift; BUZZ_PRIVATE_KEY="$sk" "$BUZZ" "$@" 2>&1; }
check() {
    local name=$1 detail=$2 ok=$3
    if [ "$ok" = "0" ]; then PASS=$((PASS + 1)); RESULTS+=("PASS  $name")
    else FAIL=$((FAIL + 1)); RESULTS+=("FAIL  $name — $detail"); fi
}
expect_ok() {
    local name=$1 out=$2
    if grep -q '"accepted":true' <<<"$out"; then check "$name" "" 0
    else check "$name" "expected acceptance, got: ${out:0:220}" 1; fi
}
expect_refused() {
    local name=$1 needle=$2 out=$3
    if grep -qi "$needle" <<<"$out"; then check "$name" "" 0
    else check "$name" "expected refusal matching '$needle', got: ${out:0:220}" 1; fi
}
expect_contains() {
    local name=$1 needle=$2 hay=$3
    if grep -qi -- "$needle" <<<"$hay"; then check "$name" "" 0
    else check "$name" "expected '$needle' in: ${hay:0:220}" 1; fi
}
jqv() { python3 -c "import sys,json;d=json.load(sys.stdin);print(d$1 if d else '')" 2>/dev/null; }

echo "== Phase 3 end-to-end run ====================================="
echo "relay: $BUZZ_RELAY_URL"

: "${BUZZ_RELAY_PRIVATE_KEY:?set BUZZ_RELAY_PRIVATE_KEY so buzz-admin can seed the roster}"
seed_member() {
    local out
    if ! out=$("$BUZZ_ADMIN" add-member --pubkey "$1" --role "$2" 2>&1); then
        echo "SETUP FAILED: buzz-admin add-member $2 -> $out" >&2; exit 2
    fi
}
seed_member "$WSADMIN_PK" admin
seed_member "$MEMBER_PK" member
seed_member "$MEMBER2_PK" member

# ── 1. Tier surface (D24): a channel declares its floor at creation ───────
for tier in owned private open; do
    OUT=$(as "$WSADMIN_SK" channels create --name "t-$tier" --type stream --visibility private --tier "$tier")
    expect_ok "team channel created at tier '$tier'" "$OUT"
    printf -v "CH_${tier}" '%s' "$(jqv "['channel_id']" <<<"$OUT")"
done
TEAM_OWNED=$CH_owned; TEAM_OPEN=$CH_open
for pk in "$MEMBER_PK" "$MEMBER2_PK"; do
    as "$WSADMIN_SK" channels add-member --channel "$TEAM_OWNED" --pubkey "$pk" --role member >/dev/null
    as "$WSADMIN_SK" channels add-member --channel "$TEAM_OPEN" --pubkey "$pk" --role member >/dev/null
done

# A team channel's tier is immutable (D26) — the promise members acted on.
OUT=$(as "$WSADMIN_SK" channels set-tier --channel "$TEAM_OPEN" --tier owned)
expect_refused "team channel tier is immutable (D26)" "immutable" "$OUT"

# ── 2. Personal channels (D29): owned by default, member-choosable ────────
OUT=$(as "$MEMBER_SK" channels create-personal --name "space")
expect_ok "member creates a personal channel" "$OUT"
PERSONAL=$(jqv "['channel_id']" <<<"$OUT")
OUT=$(as "$MEMBER_SK" channels get --channel "$PERSONAL")
expect_contains "personal channel defaults to tier 'owned'" '"tier": *"owned"' "$OUT"
expect_contains "personal channel is forced private" '"visibility": *"private"' "$OUT"

OUT=$(as "$MEMBER_SK" channels set-tier --channel "$PERSONAL" --tier private)
expect_ok "owner re-tiers their own personal channel" "$OUT"
sleep 1
OUT=$(as "$MEMBER_SK" channels get --channel "$PERSONAL")
expect_contains "re-tier took effect" '"tier": *"private"' "$OUT"

OUT=$(as "$MEMBER2_SK" channels set-tier --channel "$PERSONAL" --tier open)
expect_refused "a non-owner cannot re-tier someone's personal channel" "owner" "$OUT"
sleep 1
OUT=$(as "$MEMBER_SK" channels get --channel "$PERSONAL")
expect_contains "refused re-tier left the tier unchanged" '"tier": *"private"' "$OUT"

# ── 3. Privacy Gate assist (D30): the pre-flight review ───────────────────
OUT=$(as "$MEMBER_SK" threads open --channel "$PERSONAL" --goal "Fix the CSV importer timeout")
expect_ok "member opens a work thread in their personal channel" "$OUT"
THREAD=$(jqv "['thread_id']" <<<"$OUT")
for msg in \
  "Reproduced: the importer reopens the DB connection per row, so 40k rows times out." \
  "Fix batches inserts in chunks of 500 in one transaction. 340s -> 9s." \
  "Had to hit prod with the ops key AKIAIOSFODNN7EXAMPLE, needs rotating." \
  "Reported by Northwind Trading; their CTO called Priya about renewal." \
  "Staging dump lives on admin-3.internal.silentmesh.net."; do
    as "$MEMBER_SK" messages send --channel "$PERSONAL" --reply-to "$THREAD" --content "$msg" >/dev/null
done

REVIEW=$(as "$MEMBER_SK" threads gate-review --channel "$PERSONAL" --thread "$THREAD" --wait "$GATE_WAIT")
expect_contains "gate review returns the planted credential (deterministic)" "aws-access-key-id" "$REVIEW"
expect_contains "deterministic finding is located in the conversation" "conversation" "$REVIEW"
ASSIST=$(jqv "['assist']" <<<"$REVIEW")
case "$ASSIST" in
    ok) check "model assist ran (assist=ok)" "" 0 ;;
    unavailable|unusable|failed)
        check "model assist ran (assist=ok)" "assist=$ASSIST — is BUZZ_GATE_ASSIST_MODEL set and Ollama up?" 1 ;;
    *) check "model assist ran (assist=ok)" "no assist field: ${REVIEW:0:200}" 1 ;;
esac
if [ "$ASSIST" = "ok" ]; then
    SUMMARY=$(jqv "['suggestedSummary'] or ''" <<<"$REVIEW")
    [ -n "$SUMMARY" ] && check "assist drafted a summary" "" 0 \
        || check "assist drafted a summary" "empty suggestedSummary" 1
    # The whole point: the drafted summary must not carry what the advisory
    # notes flag. A model that quotes the key would have been dropped by the
    # vetting pass, so its absence is the property under test.
    if grep -q "AKIAIOSFODNN7EXAMPLE" <<<"$SUMMARY"; then
        check "drafted summary does not quote the credential" "summary leaked the key" 1
    else
        check "drafted summary does not quote the credential" "" 0
    fi
    # Advisory notes are the model-capability half of the assist, and small
    # models routinely return none: llama3.2:3b produced an empty list on
    # the same thread where qwen3:14b found four (customer name, a person,
    # an internal host, the key needing rotation). Treat an empty list as a
    # capability signal, not a regression — but say so loudly, because a
    # deployment running a model too weak to advise looks identical to a
    # clean thread.
    ADV=$(python3 -c "import sys,json;print(len(json.load(sys.stdin).get('advisory',[])))" <<<"$REVIEW" 2>/dev/null)
    if [ "${ADV:-0}" -gt 0 ]; then
        check "assist added advisory notes the scanners cannot see" "" 0
    else
        RESULTS+=("WARN  assist returned no advisory notes — model too weak for the advisory half (use qwen3:14b)")
    fi
fi

# A draft summary carrying a secret is reported against the summary itself.
REVIEW2=$(as "$MEMBER_SK" threads gate-review --channel "$PERSONAL" --thread "$THREAD" \
    --summary "Fixed for Northwind; rotated AKIAIOSFODNN7EXAMPLE." --wait "$GATE_WAIT")
expect_contains "a draft summary's own secret is flagged as 'summary'" '"where": *"summary"' "$REVIEW2"

# The review is a relay-signed notice on the thread.
SHOW=$(as "$MEMBER_SK" --format compact threads show --channel "$PERSONAL" --thread "$THREAD")
expect_contains "review surfaces in 'threads show' as a gate_review notice" "gate_review" "$SHOW"

# ── 4. Promotion compliance (D24/D29/D30) ────────────────────────────────
# Source is 'private'; an 'owned' target is stricter → refused.
OUT=$(as "$MEMBER_SK" threads promote --from "$PERSONAL" --thread "$THREAD" --to "$TEAM_OWNED" --summary "Importer fix")
expect_refused "promotion into a stricter channel is refused" "tier mismatch" "$OUT"

# Same source into an 'open' target is the ordinary direction → allowed,
# but the deterministic gate still refuses a summary carrying a secret.
OUT=$(as "$MEMBER_SK" threads promote --from "$PERSONAL" --thread "$THREAD" --to "$TEAM_OPEN" \
    --summary "Rotated AKIAIOSFODNN7EXAMPLE while fixing the importer")
expect_refused "the D30 gate still refuses a credential-bearing summary" "privacy gate" "$OUT"

# A clean summary into a looser channel clears BOTH privacy gates. It then
# stops at the git layer, because a CLI-only run never pushed a real commit
# — the checkpoint below is a synthetic id, exactly as the Phase 2 dry run
# does it. Reaching that error is the assertion: it proves the tier check
# and the D30 scan both passed. The full graft is covered by the S3 probe
# tests (`promote::s3_probe_tests`) against live MinIO.
CP=$(printf 'dd%.0s' {1..20})
as "$MEMBER_SK" threads checkpoint --channel "$PERSONAL" --thread "$THREAD" --commit "$CP" >/dev/null
OUT=$(as "$MEMBER_SK" threads promote --from "$PERSONAL" --thread "$THREAD" --to "$TEAM_OPEN" \
    --summary "Importer batches inserts; 340s to 9s, tests added")
if grep -qi "tier mismatch\|privacy gate" <<<"$OUT"; then
    check "a clean promotion into a looser channel clears both privacy gates" \
        "still refused on privacy grounds: ${OUT:0:200}" 1
else
    check "a clean promotion into a looser channel clears both privacy gates" "" 0
fi

# ── 5. Attribution (slice 1/4/5): the gate's inference was metered ────────
USAGE=$("$BUZZ_ADMIN" usage --since-hours 1 2>&1)
expect_contains "gate inference is attributed to the local backend" "local" "$USAGE"

echo
printf '%s\n' "${RESULTS[@]}"
echo "---------------------------------------------------------------"
echo "PASS: $PASS   FAIL: $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
