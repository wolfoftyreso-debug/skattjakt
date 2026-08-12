# Skattjakt — Client architecture

For web, Apple and Android. Neither mobile client is built (§31: no fake apps).
This is the document a team building one would start from, and the record of
what the backend already guarantees them.

The rule this is written under is §36: **build for the final architecture,
release incrementally, never architecturally prototype.** Every decision below
was made now so that building the client later is building the client, not
rewriting the core.

---

## 1. One backend, three clients

```
        Web              iOS               Android
         │                │                   │
         └────────────────┼───────────────────┘
                          │  HTTPS, one OpenAPI contract
                    ┌─────▼──────┐
                    │  API       │  authn → authz → validate → record
                    └─────┬──────┘
                          │
                    ┌─────▼──────┐
                    │  Backend   │  ALL business logic lives here
                    └────────────┘
```

**No business logic in any client.** Not "as little as possible" — none that
matters. A client renders, collects input, and calls the API. The reasons are
concrete rather than architectural taste:

- A rule about what counts as evidence, duplicated in three clients, is three
  places to fix it and two that will not be.
- An App Store review can take a week. A rule that lives in the app cannot be
  corrected faster than that.
- A client is under the user's control. Anything a client decides, a user can
  decide differently.

The line: **a client may decide what to show; only the server may decide what
is true.** Formatting money for display is a client concern. Deciding whether a
finding may be called actionable is not, and is not exposed as a field a client
could recompute.

---

## 2. What the contract already guarantees

Built and tested, with no mobile client to use it yet.

### Authentication

`POST /v1/auth/sign-in` with `x-skattjakt-client: web | ios | android`, and a
stable `install_id` the installation generates once and keeps.

Returns an access token and a refresh token. The header is not decoration — it
selects the session policy:

| | Web | iOS / Android |
|---|---|---|
| Access token | 15 min | 30 min |
| Refresh token | **12 hours** | **30 days** |
| Refresh grace | 30 s | 60 s |

The asymmetry is the platform's credential storage. A browser cannot keep a
secret away from script running on its own page; iOS has the Keychain and
Android has the Keystore. A phone app that made its user sign in twice a day
would not be used, and a browser session that lasted a month would be a
liability.

### Where a client keeps the refresh token

| Client | Where | Why |
|---|---|---|
| iOS | Keychain, `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` | Survives a reboot for background refresh; `ThisDeviceOnly` keeps it out of an iCloud backup restored onto another device |
| Android | EncryptedSharedPreferences or Keystore-wrapped | Equivalent |
| Web | `HttpOnly` `Secure` `SameSite=Strict` cookie | An XSS that can read `localStorage` gets a 30-day credential; the same XSS against an `HttpOnly` cookie gets 15 minutes of access token and no way to renew after the tab closes |

### Refresh, and the thing most implementations get wrong

Every refresh rotates. Presenting a token that was already rotated away revokes
the **whole family** — the customer and the thief both get signed out, which is
the intended outcome, because the alternative is issuing working tokens to
both.

The grace window is why this does not fire constantly on mobile. A client sends
a refresh, the server commits, the response is lost to a tunnel or a
backgrounded app, and the client retries with the only token it has: the old
one. Inside the window that is treated as a retry, not a theft.

**What a client must do:** serialise refreshes. If five requests 401 at once,
one refresh runs and the other four wait for it. Five concurrent refreshes will
look like four replays, and only the grace window stands between that and
signing the customer out.

```
  request → 401
     ├─ a refresh already in flight?  await it, then retry the request
     └─ otherwise: hold a lock, refresh, release, retry
  refresh fails → clear the tokens, show the sign-in screen. Never loop.
```

### Devices and push

A device row is created on first sign-in from an installation and survives
sign-out, so the push token and the name in the customer's device list are not
lost when someone signs out and back in.

```
PUT /v1/auth/devices/{id}/push-token   {"push_token": "...", "provider": "apns"}
PUT /v1/auth/devices/{id}/push-token   {"push_token": null}     ← notifications off
```

`GET /v1/auth/devices` is the "you are signed in on these devices" screen, with
`current: true` marking the one asking.

### Several companies, one person

An accountant with many clients is the normal case in this market. `POST
/v1/auth/switch-company` changes which tenant the session acts in, verified
against membership and without re-authenticating or rotating tokens.

A company the caller is not a member of answers **404, not 403** — whether it
exists is not their business.

---

## 3. Errors a client must handle

| Status | Meaning | What the client does |
|---|---|---|
| 401 | Access token expired or revoked | Refresh once, retry once. On a second 401, sign out |
| 403 | The role does not carry the permission | Show the message; do not retry. This is an advisor hitting an owner-only action |
| 404 | Does not exist, or belongs to another tenant — **deliberately the same answer** | Treat as gone |
| 409 | Conflict, e.g. an address already registered | Show it |
| 422 | The request was understood and is not acceptable | Show `detail`; it is written for a person |
| 429 | Rate limited or an account temporarily locked | Respect `Retry-After`. Never retry immediately |
| 5xx | The server failed | Retry with exponential backoff **and jitter**. Cap the attempts |

`detail` is safe to show a customer. `title` is a stable machine-readable
string; branch on that, never on `detail`, which is prose and will be
translated.

**Never retry a 4xx.** Nothing about the request will be different next time,
and a client that retries a 422 turns one mistake into a rate-limit lockout.

---

## 4. Long operations, and why a phone must not poll

An analysis takes minutes. A browser polling `GET /v1/analyses/{id}` every two
seconds is acceptable. A phone doing the same is not: it drains a battery for a
result that arrives once, and it will not be running when the result lands.

So the phone flow is:

```
POST /v1/analyses/stored          → 202, an analysis id
   (the app may be closed here)
   ← push notification: analysis_completed, carrying the analysis id
GET  /v1/analyses/{id}            → the result
```

The push carries **an identifier and a kind, never prose and never an amount**.
The body a customer reads is rendered on the delivery side in their language.
A lock screen is the one display surface the customer does not control:
"Din analys är klar" belongs there, "Vi hittade 186 000 kr" does not, and a
payload that *can* carry the second eventually does.

A client should still poll as a fallback — a push can be dropped by the
platform, and a notification permission can be denied — but with a long
interval and only while the relevant screen is in the foreground.

---

## 5. Uploading a document from a phone

Not the JSON body the web client posts. A 30 MB scanned annual report over a
mobile network, buffered whole in an API pod, restarting on any drop, is the
wrong shape — and it ties upload throughput to API pod memory.

```
POST /v1/documents/tickets        declared name, type, size
   → a ticket id, a storage URL, an expiry (30 minutes)
PUT  <storage URL>                the bytes, direct to storage, resumable
POST /v1/documents/tickets/{id}/complete
   → the document version id
```

The API never handles the bytes. The ticket names exactly one storage key,
derived from identifiers rather than from the filename, and is redeemed against
what actually arrived — so a ticket for a small file cannot be redeemed for a
large one.

**State:** the ticket lifecycle is implemented and tested. The S3 client that
makes the storage URL a genuine direct-to-storage URL is not written, so today a
ticket resolves through the API. That is the one piece a mobile client needs
before it ships, and it is a backend change with no contract change.

---

## 6. Offline (§20)

Analysed rather than implemented, because implementing it speculatively is how
a sync bug ships in an app nobody has used yet.

**What is genuinely worth doing offline:** letting a customer photograph or pick
their accounts on a train and having the upload complete when the network
returns. That is a queued upload, not a replicated database — the ticket flow
already supports it, since a ticket is redeemed when the bytes land, not when
the client is online.

**What is not:** analysis results. They are produced by the server, are never
edited by a client, and are worth cacheing for display but not synchronising.

**So there is no write conflict to resolve**, and that is a property of the
domain rather than an omission: the only thing a client creates is a document,
documents are immutable and content-addressed, and the same document uploaded
twice is deduplicated by hash.

If that ever stops being true — a client that edits a company profile offline —
the profile is a single JSONB document with a version, and last-write-wins with
a visible conflict is the right answer for a questionnaire. It is not
implemented, and should not be until something needs it.

---

## 7. What each platform needs beyond the contract

### iOS

- Keychain with `ThisDeviceOnly`, per §2.
- `URLSession` background upload for the ticket flow; it survives suspension,
  which is exactly the case a phone upload has to handle.
- APNs registration → `PUT /v1/auth/devices/{id}/push-token` with
  `provider: apns`. Re-register on every launch: the token changes on restore
  and after a reinstall, and a stale token is a notification that silently goes
  nowhere.
- Universal links for deep links into an analysis.
- Biometric gate on app open. A local convenience over an already-valid session,
  never an authentication factor of its own — Face ID proves someone can unlock
  the phone, which the phone already established.
- iPad: the report is a wide document and should not be a stretched phone
  layout.

### Android

- EncryptedSharedPreferences or a Keystore-wrapped key.
- `WorkManager` for the deferred upload, with constraints on network type.
- FCM → the same endpoint with `provider: fcm`. Re-register on
  `onNewToken`.
- App Links for deep links.
- Doze and background limits: a queued upload must be expressed as work the
  system schedules, not a thread that assumes it keeps running.

### Web

- **Done:** email and password, with the session in `HttpOnly` cookies. The
  page holds no credential; `authedFetch` refreshes once on a 401 and
  serialises concurrent refreshes so a slow connection cannot look like a
  replayed token.
- Loading, empty, error and degraded states for every view. The empty state
  matters more here than in most products: **finding nothing is a designed
  result**, and it must read as "we looked and found nothing", never as a
  failure or a blank list.
- Accessibility: keyboard reachable, labelled, and readable at 200% zoom. Not
  audited yet, and listed as a gap in `SKATTJAKT_PRODUCT_SURFACE.md` §4.

---

## 8. Versioning (§19)

The contract is `/v1`. The rules:

**Additive changes do not bump it.** A new optional field, a new endpoint, a new
enum value in a field documented as open. A client must ignore fields it does
not know — a client that rejects unknown fields makes every additive change
breaking.

**A bump is required for:** removing or renaming a field, narrowing a type,
adding a required request field, changing a status code's meaning, or changing
what an existing value means.

**When `/v2` arrives, `/v1` keeps working** for a stated period. Three clients
update at different speeds and one of them updates only when Apple approves it.
That asymmetry is the whole reason for the rule.

`x-skattjakt-client` also tells the server which client it is talking to, which
means a future compatibility shim can be scoped to the client that needs it
rather than applied to everyone.

---

## 9. Internationalisation (§23)

The product is Swedish and the market is Swedish. What has been kept from
becoming a rewrite later:

- All amounts are integer öre with formatting at the edge, so a currency change
  is a formatting change.
- All timestamps are `TIMESTAMPTZ` in UTC, formatted per client.
- A fiscal year is two dates, not a year — a Swedish AB may have a broken one.
- Error `title` is a stable machine string; `detail` is the prose. A client
  branches on `title`, so translating `detail` breaks nothing.

What is **not** done: customer-facing strings are Swedish literals in the code
rather than in a message catalogue. Honest position — this is the one §23 item
that would need real work to internationalise, and it is not worth doing until
there is a second market.

---

## 10. What a mobile team would need that does not exist

Stated plainly so nobody discovers it in sprint two:

1. **The upload-ticket endpoints.** The store layer and presigning are done and
   tested; no HTTP route issues a ticket yet.
2. **The push sender.** The outbox drains over email; push answers
   `NotConfigured`.
4. **A generated client SDK.** The OpenAPI file supports codegen; no pipeline
   produces or publishes one.
5. **Design.** There are no mobile designs, and this document deliberately does
   not invent them.

Items 1 and 2 are backend work with no contract change. A mobile client can be
started before they land and cannot ship without them.
