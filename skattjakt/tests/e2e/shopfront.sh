#!/usr/bin/env bash
# The six things the Swish Handel application asks a merchant to confirm.
#
# Why this is a test and not a checklist
# ======================================
#
# The application form has six checkboxes: prices, product and service
# information, terms of purchase, contact details, returns policy, returns
# information. Ticking them is an attestation to the bank that these exist on
# the site named in the form.
#
# An attestation that was true on the day it was signed and is false a month
# later is worse than one that was never made, because nobody is looking any
# more. So each of the six is asserted against a running server, and the build
# fails if one of them stops resolving or starts rendering with a gap.
#
# The gaps are the interesting part. Three of the six need facts nobody in this
# repository knows — the company's registered name, its organisationsnummer,
# its address — and a page that renders those blank looks published and attests
# to nothing. This suite therefore checks both directions: configured, the
# pages carry the real details; unconfigured, they say so rather than showing an
# empty field.
#
# Usage: tests/e2e/shopfront.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKDIR="$(mktemp -d)"
APIPORT="${APIPORT:-18105}"

# Whichever build is newer, not whichever profile is preferred. A release binary
# left over from before the pages were written passes the health check and fails
# every page — which reads as "the pages are broken" when it means "you are
# testing yesterday's binary".
API="$ROOT/target/release/skattjakt-api"
[[ -x "$API" ]] || API="$ROOT/target/debug/skattjakt-api"
DEBUG_API="$ROOT/target/debug/skattjakt-api"
[[ -x "$DEBUG_API" && "$DEBUG_API" -nt "$API" ]] && API="$DEBUG_API"

MERCHANT_NAME="Skattjakt Sverige AB"
MERCHANT_ORG="559999-1234"
MERCHANT_ADDRESS="Exempelgatan 1, 111 22 Stockholm"
MERCHANT_EMAIL="hej@skattjakt.se"

passed=0
failed=0
pass() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }

api_pid=""
cleanup() {
    [[ -n "$api_pid" ]] && kill "$api_pid" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

[[ -x "$API" ]] || { echo "build the API first: cargo build" >&2; exit 1; }

# A port already in use is a hard error rather than a silent one. Without this
# the health check below succeeds against whatever is already listening, and the
# suite tests a server it did not start — which is how a run passes for entirely
# the wrong reason.
port_is_free() {
    ! curl -sf -o /dev/null --max-time 2 "http://127.0.0.1:$APIPORT/health" 2>/dev/null
}

start_api() {
    [[ -n "$api_pid" ]] && kill "$api_pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
        port_is_free && break
        sleep 0.5
    done
    port_is_free || {
        echo "something is already listening on :$APIPORT; refusing to test it" >&2
        exit 1
    }
    env SKATTJAKT_BLOB_ROOT="$WORKDIR/blobs" PORT="$APIPORT" "$@" \
        "$API" >>"$WORKDIR/api.log" 2>&1 &
    api_pid=$!
    for _ in $(seq 1 60); do
        curl -sf "http://127.0.0.1:$APIPORT/health" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    return 1
}

body() { curl -sS --max-time 20 "http://127.0.0.1:$APIPORT$1"; }
status() { curl -sS --max-time 20 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$APIPORT$1"; }

start_api \
    SKATTJAKT_MERCHANT_NAME="$MERCHANT_NAME" \
    SKATTJAKT_MERCHANT_ORG_NUMBER="$MERCHANT_ORG" \
    SKATTJAKT_MERCHANT_ADDRESS="$MERCHANT_ADDRESS" \
    SKATTJAKT_MERCHANT_EMAIL="$MERCHANT_EMAIL" \
    SKATTJAKT_MERCHANT_VAT_REGISTERED=1 \
    || { tail -5 "$WORKDIR/api.log"; echo "the API did not start" >&2; exit 1; }

echo
echo "each of the bank's six boxes resolves"
for path in /priser /tjanster /villkor /kontakt /angerratt; do
    code="$(status "$path")"
    [[ "$code" == 200 ]] && pass "$path" || fail "$path returned $code"
done

echo
echo "and is reachable from the front page"
FRONT="$(body /)"
for path in /priser /tjanster /villkor /kontakt /angerratt; do
    grep -q "href=\"$path\"" <<<"$FRONT" \
        && pass "the front page links to $path" \
        || fail "the front page does not link to $path"
done

echo
echo "1. Prisuppgifter"
PRICES="$(body /priser)"
# The prices the code charges, for the products the code can deliver. A price
# page that disagrees with the checkout is the one failure a customer will
# certainly notice.
grep -qF "69,00 kr" <<<"$PRICES" && pass "the price 69,00 kr is published" \
    || fail "the price 69,00 kr is missing"
# 29,00 kr is Privatanalys, which this build cannot deliver — no
# private-individual rules exist — so it must NOT carry a price. When somebody
# writes the first private rule, `covers_audience` starts returning true, this
# assertion fails, and whoever wrote the rule learns that the product just went
# on sale. That is the intended way for this to change.
grep -qF "29,00 kr" <<<"$PRICES" \
    && fail "a price is published for a service this build cannot deliver" \
    || pass "no price is published for a service that cannot be bought"
grep -qF "13,80 kr" <<<"$PRICES" && pass "the VAT inside 69 kr is shown" \
    || fail "the VAT inside 69 kr is not shown"
grep -q "inklusive" <<<"$PRICES" && pass "prices are stated as including VAT" \
    || fail "prices do not say whether VAT is included"
grep -q "prenumeration" <<<"$PRICES" && pass "it says there is no subscription" \
    || fail "it does not address recurring charges"

echo
echo "2. Information om produkter och tjänster"
SERVICES="$(body /tjanster)"
for word in "Privatanalys" "Bolagsanalys" "Kontroll"; do
    grep -qF "$word" <<<"$SERVICES" && pass "$word is described" || fail "$word is missing"
done
grep -q "inte skatterådgivning\|lämnar inte skatterådgivning" <<<"$SERVICES" \
    && pass "it says what the service is not" \
    || fail "it does not say what the service is not"
grep -q "granskat av en kvalificerad" <<<"$SERVICES" \
    && pass "the unreviewed rule set is disclosed where a buyer sees it" \
    || fail "the limitation is not disclosed on the service page"
grep -q "Inte öppen för köp ännu" <<<"$SERVICES" \
    && pass "a service that cannot be delivered says so where it is described" \
    || fail "the service page advertises something nobody can buy"

# Nothing may be priced that cannot be bought. The live case: the shipped rule
# set has no private-individual rules, so Privatanalys would return an empty
# report and the customer could not tell "we found nothing" from "we looked at
# nothing". It is listed and marked closed rather than quietly hidden.
grep -q "Inte öppen för köp ännu" <<<"$PRICES" \
    && pass "a service with no rules behind it is not priced" \
    || fail "the price page prices something this build cannot deliver"
grep -q "Privatanalys" <<<"$PRICES" \
    && pass "and it is still listed rather than quietly dropped" \
    || fail "the unavailable service vanished from the price list"

echo
echo "3. Köpavtal"
TERMS="$(body /villkor)"
for topic in "Ångerrätt" "Pris och betalning" "Leverans" "Ansvarsbegränsning" "Reklamation"; do
    grep -qF "$topic" <<<"$TERMS" && pass "the terms cover $topic" || fail "the terms omit $topic"
done
grep -q "Allmänna reklamationsnämnden" <<<"$TERMS" \
    && pass "they name where a consumer can complain" \
    || fail "they do not name a dispute route"
grep -q "granskats av jurist" <<<"$TERMS" \
    && pass "they say plainly that no lawyer has read them" \
    || fail "they do not disclose that they are an unreviewed draft"

echo
echo "4. Kontaktuppgifter"
CONTACT="$(body /kontakt)"
for detail in "$MERCHANT_NAME" "$MERCHANT_ORG" "$MERCHANT_ADDRESS" "$MERCHANT_EMAIL"; do
    grep -qF "$detail" <<<"$CONTACT" && pass "the contact page carries $detail" \
        || fail "the contact page is missing $detail"
done

echo
echo "5-6. Returpolicy och returer"
RETURNS="$(body /angerratt)"
grep -q "fjorton dagars ångerrätt" <<<"$RETURNS" && pass "the fourteen days are stated" \
    || fail "the cancellation period is not stated"
grep -q "förlorar ångerrätten" <<<"$RETURNS" \
    && pass "the waiver for immediate delivery is explained" \
    || fail "the waiver is not explained"
grep -q "inte hittade något\|inte funnit något" <<<"$RETURNS" \
    && pass "it distinguishes a failed analysis from one that found nothing" \
    || fail "it does not address the found-nothing case"
grep -qF "$MERCHANT_EMAIL" <<<"$RETURNS" && pass "it says where to send a claim" \
    || fail "it does not say where to send a claim"
# The exact words the checkout records, shown where the buyer reads about the
# right they are giving up. Page and record are rendered from one constant, so
# a stored consent can never be a claim about words nobody was shown.
grep -q "Jag samtycker till att analysen påbörjas omedelbart" <<<"$RETURNS" \
    && pass "the consent wording is published verbatim" \
    || fail "the page does not show the words the checkout records"
grep -q "förlorar min ångerrätt" <<<"$RETURNS" \
    && pass "and it includes the acknowledgement the law asks for" \
    || fail "the wording omits the acknowledgement of the loss"
grep -q "Ordalydelse version" <<<"$RETURNS" \
    && pass "the wording is versioned where the buyer can see it" \
    || fail "the wording carries no version"

echo
echo "nothing renders with a gap"
# The failure this whole file exists for: a page that looks published and
# attests to nothing. An empty definition value, a doubled space where a field
# should be, or the words that would betray a template.
for path in /priser /tjanster /villkor /kontakt /angerratt; do
    page="$(body "$path")"
    if grep -qiE "<dd></dd>|TODO|FIXME|\\{\\{|XXX|placeholder|lorem" <<<"$page"; then
        fail "$path renders a gap or a placeholder"
    else
        pass "$path has no gaps"
    fi
done

echo
echo "with no merchant configured, the pages say so rather than showing blanks"
start_api || { tail -5 "$WORKDIR/api.log"; echo "restart failed" >&2; exit 1; }
for path in /priser /kontakt /villkor /angerratt /tjanster; do
    page="$(body "$path")"
    if grep -q "inte konfigurerad" <<<"$page"; then
        pass "$path is honestly unconfigured"
    else
        fail "$path rendered without merchant details and did not say so"
    fi
done
# And it must not have invented anything to fill the space.
UNSET_PAGE="$(body /kontakt)"
grep -qF "$MERCHANT_ORG" <<<"$UNSET_PAGE" && fail "an unconfigured page carried details" \
    || pass "an unconfigured page invents nothing"

printf '\npassed %d, failed %d\n' "$passed" "$failed"
[[ "$failed" -eq 0 ]] || exit 1
echo "the six things the payment scheme asks about are published and true"
